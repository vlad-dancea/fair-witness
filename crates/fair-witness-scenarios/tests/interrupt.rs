//! Interrupts the runner in the middle of a batch, as Ctrl+C does.

#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Long enough for a run to end on a slow machine.
const PATIENCE: Duration = Duration::from_secs(20);

/// What the stand-in for fair-witness prints: a change set and its verdict.
const EVENTS: &str = r#"{"time":"2026-10-27T11:00:00Z","event":"changes","set":1,"changes":[{"created":"notes.txt"}]}
{"time":"2026-10-27T11:00:01Z","event":"verdict","set":1,"verdict":{"contradicts":false,"explanation":"Fine."}}
"#;

/// The runner, killed if the test fails before it exits.
struct Runner(Child);

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// An edge case with one change set that `script` waits for.
fn scenario(root: &Path, name: &str, script: &str) {
    let dir = root.join("edge-cases").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("scenario.toml"),
        "description = \"\"\n[[set]]\ncontradicts = false\nexplanation = \"Fine.\"\n",
    )
    .unwrap();
    fs::write(dir.join("dmp.json"), "{}").unwrap();
    fs::write(dir.join("script.sh"), script).unwrap();
}

/// What `poll` gives once it gives something, within `PATIENCE`.
fn wait_for<T>(what: &str, mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(value) = poll() {
            return value;
        }
        assert!(Instant::now() < deadline, "{what} took over {PATIENCE:?}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// The `results.json` in the one folder in `output`, once there is one.
fn results(output: &Path) -> Option<Value> {
    let folder = fs::read_dir(output).ok()?.next()?.ok()?.path();
    serde_json::from_str(&fs::read_to_string(folder.join("results.json")).ok()?).ok()
}

/// The status in `results` and the name of each run with whether it passed.
fn summary(results: &Value) -> (&str, Vec<(&str, bool)>) {
    let runs = results["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| {
            (
                run["name"].as_str().unwrap(),
                run["passed"].as_bool().unwrap(),
            )
        })
        .collect();
    (results["status"].as_str().unwrap(), runs)
}

#[test]
fn keeps_the_runs_that_ended_when_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let witness = dir.path().join("fair-witness");
    fs::write(
        &witness,
        format!(
            "#!/bin/sh\necho 'Watching the folder' >&2\nprintf '%s' '{EVENTS}'\ncat >/dev/null\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&witness, fs::Permissions::from_mode(0o755)).unwrap();
    let scenarios = dir.path().join("scenarios");
    let held = dir.path().join("held");
    scenario(&scenarios, "a-first", "wait_for_verdict\n");
    // Sleeps rather than waiting on the test, so that it ends even if nothing stops it.
    scenario(
        &scenarios,
        "b-held",
        &format!(
            "echo $$ > '{}'\nsleep 30\nwait_for_verdict\n",
            held.display()
        ),
    );
    scenario(&scenarios, "c-last", "wait_for_verdict\n");
    let output = dir.path().join("output");
    let mut runner = Runner(
        Command::new(env!("CARGO_BIN_EXE_fair-witness-scenarios"))
            .arg("--scenarios")
            .arg(&scenarios)
            .arg("--binary")
            .arg(&witness)
            .arg("--output")
            .arg(&output)
            .args(["--jobs", "1", "--debounce-ms", "10"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );

    // The process group of the held script, which its bash leads.
    let group = wait_for("the second run", || {
        fs::read_to_string(&held)
            .ok()
            .filter(|group| group.ends_with('\n'))
    });
    let running = results(&output).unwrap();
    Command::new("kill")
        .args(["-s", "INT", &runner.0.id().to_string()])
        .status()
        .unwrap();
    let status = wait_for("the exit", || runner.0.try_wait().unwrap());
    let mut stderr = String::new();
    runner
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let interrupted = results(&output).unwrap();

    assert_eq!(status.code(), Some(130), "{stderr}");
    assert_eq!(summary(&running), ("running", vec![("a-first", true)]));
    assert_eq!(
        summary(&interrupted),
        ("interrupted", vec![("a-first", true)])
    );
    let folder = fs::read_dir(&output)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let events = interrupted["runs"][0]["events"].as_str().unwrap();
    assert_eq!(fs::read_to_string(folder.join(events)).unwrap(), EVENTS);
    // Gone, or killed and not yet reaped.
    wait_for("stopping the held script", || {
        let ps = Command::new("ps")
            .args(["-o", "stat=", "-p", group.trim()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&ps.stdout);
        (stat.trim().is_empty() || stat.starts_with('Z')).then_some(())
    });
}
