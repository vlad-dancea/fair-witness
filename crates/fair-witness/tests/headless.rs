//! Runs the binary headless, as a script or an agent would.

use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use fair_witness_core::project::{Config, DamapConfig, Project};
use fair_witness_core::testing::{fake_model, tool_call};
use serde_json::{Value, json};

/// Long enough for the watcher to wait out its debounce and the review to finish.
const PATIENCE: Duration = Duration::from_secs(10);

const DMP: &str = r#"{"dmp": {"title": "Soil samples", "dataset": [{"personal_data": "no"}]}}"#;

struct Run {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Receiver<String>,
    /// stdout when nothing reads it, kept so the pipe stays open.
    unread: Option<ChildStdout>,
    stderr: Receiver<String>,
}

impl Run {
    /// Starts `fair-witness run --headless` in the folder `here`.
    fn start(here: &Path, arguments: &[&str]) -> Self {
        let mut run = Self::start_unread(here, arguments);
        run.stdout = lines(run.unread.take().unwrap());
        run
    }

    /// Like `start`, but nothing reads stdout, as with a reader that stopped reading.
    fn start_unread(here: &Path, arguments: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fair-witness"))
            .current_dir(here)
            .arg("run")
            .arg("--headless")
            .args(arguments)
            .env("FAIR_WITNESS_DEBOUNCE_MS", "50")
            .env_remove("FAIR_WITNESS_AI_URL")
            .env_remove("FAIR_WITNESS_API_KEY")
            .env_remove("FAIR_WITNESS_MODEL")
            .env_remove("FAIR_WITNESS_EFFORT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            stdin: child.stdin.take(),
            stdout: mpsc::channel().1,
            unread: child.stdout.take(),
            stderr: lines(child.stderr.take().unwrap()),
            child,
        }
    }

    /// The lines on stderr until the one that starts with `prefix`.
    fn stderr_until(&self, prefix: &str) -> Vec<String> {
        let mut seen = Vec::new();
        loop {
            let line = self
                .stderr
                .recv_timeout(PATIENCE)
                .unwrap_or_else(|_| panic!("no {prefix:?} on stderr in time, only {seen:?}"));
            let done = line.starts_with(prefix);
            seen.push(line);
            if done {
                return seen;
            }
        }
    }

    /// The events on stdout up to the verdict, without the time each line has.
    fn events_until_verdict(&self) -> Vec<Value> {
        let mut events = Vec::new();
        loop {
            let line = self
                .stdout
                .recv_timeout(PATIENCE)
                .expect("no verdict in time");
            let mut event: Value = serde_json::from_str(&line).unwrap();
            let time = event.as_object_mut().unwrap().remove("time");
            assert!(time.is_some_and(|time| time.is_string()), "{line}");
            let done = event["event"] == "verdict";
            events.push(event);
            if done {
                return events;
            }
        }
    }

    /// Closes stdin and returns the exit code.
    fn stop(mut self) -> Option<i32> {
        drop(self.stdin.take());
        self.exit_code()
    }

    fn exit_code(&mut self) -> Option<i32> {
        for _ in 0..PATIENCE.as_millis() / 20 {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.child.kill().unwrap();
        panic!("fair-witness did not stop in time");
    }
}

fn lines(output: impl Read + Send + 'static) -> Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if sender.send(line.unwrap()).is_err() {
                return;
            }
        }
    });
    receiver
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn prints_the_events_of_a_folder_that_was_never_set_up() {
    let parent = tempfile::tempdir().unwrap();
    let parent = parent.path().canonicalize().unwrap();
    // The current folder is set up, and `..` must not lead back to it.
    let current = parent.join("current");
    Project::new(current.clone()).create().unwrap();
    let project = parent.join("sibling");
    std::fs::create_dir(&project).unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let dmp_file = elsewhere.path().join("dmp.json");
    std::fs::write(&dmp_file, DMP).unwrap();
    let (endpoint, server) = fake_model(vec![tool_call(
        "report_verdict",
        json!({
            "contradicts": true,
            "dmp_field": "dataset[0].personal_data",
            "explanation": "Patient names, but the DMP says no personal data."
        }),
    )]);

    let run = Run::start(
        &current,
        &[
            "--project",
            "../sibling",
            "--dmp-file",
            path(&dmp_file),
            "--ai-url",
            &endpoint,
            "--model",
            "test",
            "--effort",
            "ultra",
        ],
    );
    assert_eq!(
        run.stderr_until("Comparing"),
        [
            format!("Watching {}", project.display()),
            format!("Comparing changes with the DMP in {}.", dmp_file.display()),
        ]
    );
    std::fs::write(project.join("patients.csv"), "name\nAnna Muster\n").unwrap();
    let events = run.events_until_verdict();

    let kinds: Vec<&str> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "changes",
            "reviewing",
            "trace",
            "trace",
            "trace",
            "trace",
            "trace",
            "verdict"
        ]
    );
    assert_eq!(
        events[0],
        json!({ "event": "changes", "set": 1, "changes": [{ "created": "patients.csv" }] })
    );
    assert_eq!(
        events[7],
        json!({
            "event": "verdict",
            "set": 1,
            "verdict": {
                "contradicts": true,
                "dmp_field": "dataset[0].personal_data",
                "explanation": "Patient names, but the DMP says no personal data."
            }
        })
    );
    assert!(!project.join(".fair-witness").exists());
    let requests = server.join().unwrap();
    assert!(requests[0].1.to_string().contains("Soil samples"));
    assert_eq!(requests[0].1["reasoning_effort"], "ultra");
    assert_eq!(run.stop(), Some(0));
}

#[test]
fn checks_against_the_saved_copy_when_damap_cannot_be_reached() {
    let root = tempfile::tempdir().unwrap();
    let project = Project::new(root.path().to_path_buf());
    // A port that was just free, so DAMAP cannot be reached.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let config = Config {
        damap: DamapConfig {
            frontend_url: format!("http://127.0.0.1:{port}"),
            dmp_id: Some(3),
        },
        ai: None,
    };
    project.save_config(&config).unwrap();
    project
        .save_madmp(
            &config.damap.frontend_url,
            3,
            &serde_json::from_str(DMP).unwrap(),
        )
        .unwrap();
    let (endpoint, server) = fake_model(vec![tool_call(
        "report_verdict",
        json!({ "contradicts": false, "explanation": "Notes are not data." }),
    )]);
    let ai = ["--ai-url", &endpoint, "--model", "test"];

    let mut other_dmp = Run::start(root.path(), &[&["--dmp", "4"], &ai[..]].concat());
    assert_eq!(other_dmp.exit_code(), Some(2));
    let error = other_dmp.stderr_until("error:").pop().unwrap();
    assert!(error.starts_with("error: could not reach"), "{error}");

    let run = Run::start(root.path(), &ai);
    let status = run.stderr_until("Warning:");
    assert_eq!(
        status[..2],
        [
            format!("Watching {}", root.path().display()),
            "Comparing changes with the saved copy of project #3.".to_string(),
        ]
    );
    assert!(
        status[2].starts_with(
            "Warning: could not download the DMP, so changes are checked against the copy \
             saved before: could not reach"
        ),
        "{}",
        status[2]
    );
    std::fs::write(root.path().join("notes.txt"), "Ask about consent").unwrap();
    let events = run.events_until_verdict();
    assert_eq!(events.last().unwrap()["verdict"]["contradicts"], false);
    assert!(
        server.join().unwrap()[0]
            .1
            .to_string()
            .contains("Soil samples")
    );
    assert_eq!(run.stop(), Some(0));
    // A folder that is set up keeps a log, with the same lines that went to stdout. The run
    // that could not start left one too, which is older.
    let mut logs: Vec<_> = std::fs::read_dir(root.path().join(".fair-witness/log"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with("-events.jsonl"))
        .collect();
    logs.sort();
    let logged: Vec<Value> = std::fs::read_to_string(logs.last().unwrap())
        .unwrap()
        .lines()
        .map(|line| {
            let mut event: Value = serde_json::from_str(line).unwrap();
            event.as_object_mut().unwrap().remove("time");
            event
        })
        .collect();
    assert_eq!(logged, events);
}

#[test]
fn exits_with_2_when_it_cannot_start() {
    let root = tempfile::tempdir().unwrap();
    let mut run = Run::start(root.path(), &[]);
    assert_eq!(run.exit_code(), Some(2));
    assert_eq!(
        run.stderr_until("error:"),
        ["error: this folder is not set up yet; run `fair-witness setup` first"]
    );
}

#[test]
fn stops_while_it_waits_for_damap() {
    let root = tempfile::tempdir().unwrap();
    // Takes the connection but never answers, as a DAMAP that hangs would.
    let damap = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = Config {
        damap: DamapConfig {
            frontend_url: format!("http://{}", damap.local_addr().unwrap()),
            dmp_id: Some(3),
        },
        ai: None,
    };
    Project::new(root.path().to_path_buf())
        .save_config(&config)
        .unwrap();

    let run = Run::start(
        root.path(),
        &["--ai-url", "http://127.0.0.1:9/v1", "--model", "test"],
    );

    assert_eq!(run.stop(), Some(0));
}

/// Starts watching `project` against a DMP larger than a pipe holds, which the first trace
/// event of a review carries. Nothing reads stdout.
fn start_with_a_large_dmp(project: &Path) -> Run {
    let elsewhere = tempfile::tempdir().unwrap();
    let dmp_file = elsewhere.path().join("dmp.json");
    let title = "x".repeat(1 << 20);
    std::fs::write(&dmp_file, json!({ "dmp": { "title": title } }).to_string()).unwrap();
    let (endpoint, _server) = fake_model(vec![tool_call(
        "report_verdict",
        json!({ "contradicts": false, "explanation": "Notes are not data." }),
    )]);
    let run = Run::start_unread(
        project,
        &[
            "--dmp-file",
            path(&dmp_file),
            "--ai-url",
            &endpoint,
            "--model",
            "test",
        ],
    );
    run.stderr_until("Comparing");
    run
}

/// Starts reviewing a change and reads the first part of the event that carries the large DMP,
/// so that the writer is stuck in the middle of it once nothing reads any more.
fn stuck_writing(project: &Path, run: &mut Run) -> Vec<u8> {
    std::fs::write(project.join("notes.txt"), "Ask about consent").unwrap();
    let mut read = vec![0; 128 << 10];
    run.unread.as_mut().unwrap().read_exact(&mut read).unwrap();
    read
}

#[test]
fn stops_with_whole_lines_when_its_events_are_read_late() {
    let project = tempfile::tempdir().unwrap();
    let mut run = start_with_a_large_dmp(project.path());
    let mut stdout = stuck_writing(project.path(), &mut run);

    drop(run.stdin.take());
    // Time to see that stdin closed while still in the middle of a line.
    thread::sleep(Duration::from_millis(300));
    run.unread.take().unwrap().read_to_end(&mut stdout).unwrap();

    assert_eq!(run.exit_code(), Some(0));
    let events: Vec<Value> = String::from_utf8(stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let large = events
        .iter()
        .filter(|event| event.to_string().len() > 1 << 20);
    assert_eq!(large.count(), 1);
}

#[test]
fn stops_when_nobody_reads_its_events() {
    let project = tempfile::tempdir().unwrap();
    let mut run = start_with_a_large_dmp(project.path());
    stuck_writing(project.path(), &mut run);
    drop(run.stdin.take());

    assert_eq!(run.exit_code(), Some(1));
    assert_eq!(
        run.stderr_until("error:").pop().unwrap(),
        "error: stopped before writing every event, since stdout was not read"
    );
}
