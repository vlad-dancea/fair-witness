use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fair_witness_core::project::{AiConfig, Effort};
use fair_witness_core::review::{Checked, ModelChoice, Reviewer};
use fair_witness_core::secret::hide;
use fair_witness_core::session::{Event, Record};
use fair_witness_core::testing::fake_model;

use crate::report::Refused;
use crate::scenario::{DMP, FILES, SCRIPT, Scenario};

/// Generous, so that a slow CI machine still makes it, while a scenario that hangs fails.
const STARTUP: Duration = Duration::from_secs(20);
const FAKE_REVIEW: Duration = Duration::from_secs(20);
/// Generous, since a real model's review can take up to 12 replies at a high effort from a busy
/// API, while a review that hangs still fails.
const REAL_REVIEW: Duration = Duration::from_secs(10 * 60);
const SCRIPT_STEP: Duration = Duration::from_secs(60);
const STOP: Duration = Duration::from_secs(10);

/// fair-witness reads the API key from here, so that it is not on a command line.
pub const API_KEY: &str = "FAIR_WITNESS_API_KEY";

/// The line `wait_for_verdict` prints to hand the folder over for review.
const WAIT: &str = "::fair-witness-scenarios::wait_for_verdict::";
/// Starts the line that gives the script's exit code once it has ended.
const ENDED: &str = "::fair-witness-scenarios::ended::";

/// The process groups of the scripts that are running, or none once the runner is stopping.
static GROUPS: Mutex<Option<Vec<u32>>> = Mutex::new(Some(Vec::new()));

/// How to run fair-witness for every scenario.
pub struct Settings {
    pub binary: PathBuf,
    pub debounce: Duration,
    /// The key for the API of the real models, which only the checks and fair-witness get.
    pub api_key: Option<String>,
}

/// The model that reviews the change sets of a run.
#[derive(Debug, Clone, PartialEq)]
pub enum Model {
    /// The fake model from `testing.rs`, which makes the scenario's tool calls and hands in its
    /// expected verdict.
    Fake,
    /// A model behind an OpenAI-compatible API, whose verdicts are measured.
    Real {
        url: String,
        name: String,
        effort: Effort,
    },
}

impl Model {
    pub fn name(&self) -> &str {
        match self {
            Model::Fake => "fake",
            Model::Real { name, .. } => name,
        }
    }

    pub fn effort(&self) -> Effort {
        match self {
            Model::Fake => Effort::default(),
            Model::Real { effort, .. } => effort.clone(),
        }
    }

    /// The folder for the events of its runs, named after the model and effort with what a file
    /// name cannot hold, such as the `/` in `openai/gpt-5`, replaced.
    pub fn folder(&self) -> String {
        match self {
            Model::Fake => "fake".to_string(),
            Model::Real { name, effort, .. } => {
                format!("{}-{}", file_name(name), file_name(effort.name()))
            }
        }
    }
}

fn file_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

/// Asks the API once whether it takes each real model at its effort, as fair-witness does when it
/// starts, and gives back the ones it refused, which are not worth a run. Says on stderr which
/// ones the check could not tell about, which run anyway.
pub fn check(models: &[Model], settings: &Settings) -> Result<Vec<Refused>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start checking the models")?;
    let key = settings.api_key.as_deref();
    // One reviewer for each API, which remembers the models it asked to allow an effort for.
    let mut reviewers: HashMap<&str, Reviewer> = HashMap::new();
    let mut refused = Vec::new();
    for model in models {
        let Model::Real { url, name, effort } = model else {
            continue;
        };
        let reviewer = match reviewers.entry(url) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let ai = AiConfig {
                    url: url.clone(),
                    model: name.clone(),
                    effort: effort.clone(),
                };
                entry.insert(Reviewer::new(&ai, settings.api_key.clone())?)
            }
        };
        let choice = ModelChoice {
            model: name.clone(),
            effort: effort.clone(),
        };
        match runtime.block_on(reviewer.check(&choice)) {
            Checked::Accepted => {}
            Checked::Refused(error) => refused.push(Refused {
                model: name.clone(),
                effort: effort.clone(),
                error: hide(error, key),
            }),
            Checked::Unknown(why) => eprintln!(
                "Could not check {name} {}, so it runs anyway: {}",
                effort.name(),
                hide(why, key)
            ),
        }
    }
    Ok(refused)
}

/// What a run of a scenario produced: every event fair-witness printed and, when the run went
/// wrong or stopped early, why.
pub struct Ran {
    pub records: Vec<Record>,
    pub error: Option<String>,
}

/// Copies the scenario's files into a new folder, watches it with headless fair-witness and
/// `model`, and runs the script, which stops after each change set until it is reviewed.
pub fn run(scenario: &Scenario, model: &Model, settings: &Settings) -> Ran {
    let mut records = Vec::new();
    let error = run_into(scenario, model, settings, &mut records)
        .err()
        .map(|e| format!("{e:#}"));
    Ran { records, error }
}

fn run_into(
    scenario: &Scenario,
    model: &Model,
    settings: &Settings,
    records: &mut Vec<Record>,
) -> Result<()> {
    let folder = tempfile::Builder::new()
        .prefix("fair-witness-scenario-")
        .tempdir()
        .context("could not create the project folder")?;
    let files = scenario.dir.join(FILES);
    if files.is_dir() {
        copy_dir(&files, folder.path())?;
    }
    let (url, review) = match model {
        Model::Fake => (fake_model(scenario.fake_replies()).0, FAKE_REVIEW),
        Model::Real { url, .. } => (url.clone(), REAL_REVIEW),
    };
    let mut witness = Process::start(
        "fair-witness",
        &mut witness_command(scenario, folder.path(), &url, model, settings),
        settings.api_key.clone(),
    )
    .with_context(|| format!("could not start {}", settings.binary.display()))?;
    witness.until_watching()?;

    let mut script = Process::start_group(SCRIPT, &mut script_command(scenario, folder.path()))
        .context("could not start bash")?;
    let mut set = 0;
    let mut deadline = Instant::now() + SCRIPT_STEP;
    let code = loop {
        match script.stdout.recv_timeout(left(deadline)) {
            Ok(line) if line == WAIT => {
                set += 1;
                if set > scenario.sets.len() {
                    bail!(
                        "{SCRIPT} waits for more verdicts than scenario.toml has sets ({})",
                        scenario.sets.len()
                    );
                }
                witness.until_decided(set, review, records)?;
                script.answer();
                deadline = Instant::now() + SCRIPT_STEP;
            }
            Ok(line) => {
                if let Some(code) = line.strip_prefix(ENDED) {
                    break code.parse::<i32>().ok();
                }
                // Otherwise it is the script's own output.
            }
            Err(RecvTimeoutError::Timeout) => {
                bail!("{SCRIPT} neither waited for a verdict nor finished within {SCRIPT_STEP:?}")
            }
            // bash ended without saying how the script did.
            Err(RecvTimeoutError::Disconnected) => break None,
        }
    };
    // What the script left running in the background stops with it, so that it changes nothing
    // later and does not hold its output open.
    script.stop_group();
    match code {
        Some(0) => {}
        Some(code) => bail!("{SCRIPT} failed with exit code {code}{}", script.errors()),
        None => bail!(script.stopped()?),
    }
    if set < scenario.sets.len() {
        bail!(
            "{SCRIPT} waited for {set} verdicts, but scenario.toml expects {}",
            scenario.sets.len()
        );
    }
    // A set of changes the script made after it last waited comes two and a half debounces
    // after the last of them.
    witness.collect_for(
        settings.debounce * 5 / 2 + Duration::from_millis(200),
        records,
    )?;
    witness.stop(records)
}

/// Headless fair-witness watching `folder` and reviewing with `model` at `url`.
fn witness_command(
    scenario: &Scenario,
    folder: &Path,
    url: &str,
    model: &Model,
    settings: &Settings,
) -> Command {
    let mut command = Command::new(&settings.binary);
    command
        .args(["run", "--headless", "--project"])
        .arg(folder)
        .arg("--dmp-file")
        .arg(scenario.dir.join(DMP))
        .args(["--ai-url", url, "--model", model.name()])
        // In one argument, so that a name starting with `-` is not read as a flag.
        .arg(format!("--effort={}", model.effort().name()))
        // `check` asked the API about each real model and effort before all their runs, and the
        // fake model answers only what the scenario expects.
        .arg("--skip-model-check")
        .env(
            "FAIR_WITNESS_DEBOUNCE_MS",
            settings.debounce.as_millis().to_string(),
        );
    match (model, &settings.api_key) {
        (Model::Real { .. }, Some(key)) => command.env(API_KEY, key),
        // A key meant for a real model must not go to the fake one.
        _ => command.env_remove(API_KEY),
    };
    command
}

/// bash running the scenario's script in `folder`.
fn script_command(scenario: &Scenario, folder: &Path) -> Command {
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(prelude())
        .arg(scenario.dir.join(SCRIPT))
        .current_dir(folder)
        // The script needs no key, and its output could show it.
        .env_remove(API_KEY);
    command
}

/// Defines `wait_for_verdict` and runs the script given as `$0` in a subshell. bash then says how
/// the script ended and waits to be killed with the process group it leads, since a group whose
/// leader is reaped first can lose its id to another one.
fn prelude() -> String {
    format!(
        "wait_for_verdict() {{\n    echo '{WAIT}'\n    read -r _\n}}\n\
         (\n    set -euo pipefail\n    . \"$0\"\n)\n\
         echo \"{ENDED}$?\"\n\
         read -r _\n"
    )
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    let entries =
        fs::read_dir(from).with_context(|| format!("could not read {}", from.display()))?;
    for entry in entries {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir(&target)?;
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)
                .with_context(|| format!("could not copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Stops every script that is running, with what it started, and any script that would start
/// later, for a runner that is being stopped.
pub fn stop_scripts() {
    let groups = GROUPS.lock().unwrap_or_else(PoisonError::into_inner).take();
    for group in groups.into_iter().flatten() {
        kill_group(group);
    }
}

/// Kills the process group `group` and everything in it.
fn kill_group(group: u32) {
    let _ = Command::new("kill")
        .args(["-s", "KILL", "--", &format!("-{group}")])
        .stderr(Stdio::null())
        .status();
}

/// A child process whose output is read line by line. It is killed when dropped, so a scenario
/// that goes wrong leaves nothing running.
struct Process {
    name: &'static str,
    /// The process group the process leads, if it has one of its own.
    group: Option<u32>,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Receiver<String>,
    stderr: Receiver<String>,
    /// The lines read from stderr so far.
    stderr_lines: Vec<String>,
}

impl Process {
    /// Starts `command` and reads its output with `secret` hidden.
    fn start(name: &'static str, command: &mut Command, secret: Option<String>) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Self {
            name,
            group: None,
            stdin: child.stdin.take(),
            stdout: lines(
                child.stdout.take().expect("stdout is piped"),
                secret.clone(),
            ),
            stderr: lines(child.stderr.take().expect("stderr is piped"), secret),
            stderr_lines: Vec::new(),
            child,
        })
    }

    /// Starts `command` in a process group of its own, so that `stop_group` stops what it starts
    /// in the background too. Ctrl+C in the terminal does not reach the group, so `stop_scripts`
    /// stops it instead.
    fn start_group(name: &'static str, command: &mut Command) -> Result<Self> {
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(command, 0);
        // Held while starting, so that `stop_scripts` cannot miss the new group.
        let mut groups = GROUPS.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(groups) = groups.as_mut() else {
            bail!("the runner is stopping");
        };
        let mut process = Self::start(name, command, None)?;
        // The leader's process id is the id of its group.
        let group = process.child.id();
        groups.push(group);
        process.group = Some(group);
        Ok(process)
    }

    /// Kills the process group, if the process leads one. It must not have been reaped yet.
    fn stop_group(&mut self) {
        let Some(group) = self.group.take() else {
            return;
        };
        // Killed while the lock is held, so that a `stop_scripts` in between cannot miss it.
        let mut groups = GROUPS.lock().unwrap_or_else(PoisonError::into_inner);
        kill_group(group);
        if let Some(groups) = groups.as_mut() {
            groups.retain(|running| *running != group);
        }
    }

    /// Waits for fair-witness to say that it watches the folder, which it does once its
    /// watcher is ready.
    fn until_watching(&mut self) -> Result<()> {
        let deadline = Instant::now() + STARTUP;
        loop {
            match self.stderr.recv_timeout(left(deadline)) {
                Ok(line) => {
                    let watching = line.starts_with("Watching ");
                    self.stderr_lines.push(line);
                    if watching {
                        return Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    bail!("fair-witness did not start watching within {STARTUP:?}")
                }
                Err(RecvTimeoutError::Disconnected) => bail!(self.stopped()?),
            }
        }
    }

    /// Reads events until change set `set` has its verdict or its review failed, for at most
    /// `review`.
    fn until_decided(
        &mut self,
        set: usize,
        review: Duration,
        records: &mut Vec<Record>,
    ) -> Result<()> {
        let deadline = Instant::now() + review;
        let mut changed = false;
        loop {
            let record = match self.next_record(deadline)? {
                Some(record) => record,
                None if changed => bail!("change set {set} got no verdict within {review:?}"),
                None => bail!(
                    "fair-witness saw no changes before {SCRIPT} waited for verdict {set}, \
                     within {review:?}"
                ),
            };
            let decided = match &record.event {
                Event::Changes { set: of, .. } => {
                    changed |= *of == set;
                    false
                }
                Event::Verdict { set: of, .. } | Event::ReviewFailed { set: of, .. } => *of == set,
                _ => false,
            };
            records.push(record);
            if decided {
                return Ok(());
            }
        }
    }

    /// Reads events for `duration`.
    fn collect_for(&mut self, duration: Duration, records: &mut Vec<Record>) -> Result<()> {
        let deadline = Instant::now() + duration;
        while let Some(record) = self.next_record(deadline)? {
            records.push(record);
        }
        Ok(())
    }

    /// The next event, or none if there is none before `deadline`.
    fn next_record(&mut self, deadline: Instant) -> Result<Option<Record>> {
        match self.stdout.recv_timeout(left(deadline)) {
            Ok(line) => serde_json::from_str(&line)
                .map(Some)
                .with_context(|| format!("fair-witness printed a line that is no event: {line}")),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => bail!(self.stopped()?),
        }
    }

    /// Closes stdin, which stops fair-witness, and keeps the events it prints until then.
    fn stop(mut self, records: &mut Vec<Record>) -> Result<()> {
        drop(self.stdin.take());
        let deadline = Instant::now() + STOP;
        while let Ok(line) = self.stdout.recv_timeout(left(deadline)) {
            let record = serde_json::from_str(&line)
                .with_context(|| format!("fair-witness printed a line that is no event: {line}"))?;
            records.push(record);
        }
        if !self.exit_status()?.success() {
            bail!(self.stopped()?);
        }
        Ok(())
    }

    /// Lets `wait_for_verdict` return.
    fn answer(&mut self) {
        // A script that has stopped reading has ended, which the next line it prints shows.
        if let Some(stdin) = &mut self.stdin {
            let _ = stdin.write_all(b"\n");
        }
    }

    /// Waits for the process to exit, and kills it if it does not.
    fn exit_status(&mut self) -> Result<ExitStatus> {
        let deadline = Instant::now() + STOP;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                bail!("{} did not exit within {STOP:?}", self.name);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Says how the process stopped and what it wrote to stderr.
    fn stopped(&mut self) -> Result<String> {
        let status = self.exit_status()?;
        let errors = self.errors();
        Ok(format!("{} stopped with {status}{errors}", self.name))
    }

    /// What the process wrote to stderr, to follow an error message.
    fn errors(&mut self) -> String {
        // Once the process has exited, the rest of stderr comes right away, unless something it
        // started keeps stderr open.
        let deadline = Instant::now() + Duration::from_secs(1);
        while let Ok(line) = self.stderr.recv_timeout(left(deadline)) {
            self.stderr_lines.push(line);
        }
        if self.stderr_lines.is_empty() {
            String::new()
        } else {
            format!(":\n{}", self.stderr_lines.join("\n"))
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop_group();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The lines of `output`, with `secret` hidden in them.
fn lines(output: impl Read + Send + 'static, secret: Option<String>) -> Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if sender.send(hide(line, secret.as_deref())).is_err() {
                return;
            }
        }
    });
    receiver
}

fn left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use fair_witness_core::testing::{EFFORT_REFUSED, bad_request, one_token};

    use super::*;

    fn scenario() -> Scenario {
        Scenario {
            name: "notes".to_string(),
            dir: PathBuf::from("/scenarios/notes"),
            description: String::new(),
            sets: Vec::new(),
        }
    }

    /// What `command` does with the API key: `None` when it inherits it, `Some(None)` when it
    /// removes it and `Some(Some(key))` when it sets it.
    fn api_key(command: &Command) -> Option<Option<&str>> {
        command
            .get_envs()
            .find(|(name, _)| *name == API_KEY)
            .map(|(_, value)| value.map(|value| value.to_str().unwrap()))
    }

    #[test]
    fn hands_the_api_key_to_fair_witness_alone_and_only_for_a_real_model() {
        let settings = Settings {
            binary: PathBuf::from("fair-witness"),
            debounce: Duration::from_millis(500),
            api_key: Some("sk-secret".to_string()),
        };
        let real = Model::Real {
            url: "https://api.example.org/v1".to_string(),
            name: "large".to_string(),
            effort: "high".parse().unwrap(),
        };
        let folder = Path::new("/tmp/project");

        let witness = witness_command(
            &scenario(),
            folder,
            "https://api.example.org/v1",
            &real,
            &settings,
        );
        let fake = witness_command(
            &scenario(),
            folder,
            "http://127.0.0.1:9/v1",
            &Model::Fake,
            &settings,
        );

        let arguments: Vec<&str> = witness
            .get_args()
            .map(|argument| argument.to_str().unwrap())
            .collect();
        assert_eq!(
            arguments,
            [
                "run",
                "--headless",
                "--project",
                "/tmp/project",
                "--dmp-file",
                "/scenarios/notes/dmp.json",
                "--ai-url",
                "https://api.example.org/v1",
                "--model",
                "large",
                "--effort=high",
                "--skip-model-check"
            ]
        );
        assert_eq!(api_key(&witness), Some(Some("sk-secret")));
        assert_eq!(api_key(&fake), Some(None));
        assert_eq!(api_key(&script_command(&scenario(), folder)), Some(None));
    }

    #[test]
    fn reports_the_models_and_efforts_the_api_refuses_with_its_message() {
        let gone = "The model gone is not available with the key sk-secret.";
        let (url, server) = fake_model(vec![
            bad_request(EFFORT_REFUSED),
            bad_request(EFFORT_REFUSED),
            one_token().into(),
            bad_request(gone),
            bad_request(gone),
        ]);
        let model = |name: &str, effort: &str| Model::Real {
            url: url.clone(),
            name: name.to_string(),
            effort: effort.parse().unwrap(),
        };
        let settings = Settings {
            binary: PathBuf::from("fair-witness"),
            debounce: Duration::from_millis(500),
            api_key: Some("sk-secret".to_string()),
        };

        let refused = check(
            &[
                Model::Fake,
                model("qwen", "high"),
                model("qwen", "xhigh"),
                model("gone", "default"),
            ],
            &settings,
        )
        .unwrap();

        assert_eq!(
            refused,
            [
                Refused {
                    model: "qwen".to_string(),
                    effort: "high".parse().unwrap(),
                    error: EFFORT_REFUSED.to_string(),
                },
                Refused {
                    model: "gone".to_string(),
                    effort: Effort::default(),
                    error: "The model gone is not available with the key [API key].".to_string(),
                },
            ]
        );
        let requests = server.join().unwrap();
        assert!(
            requests
                .iter()
                .all(|(head, _)| head.contains("authorization: Bearer sk-secret")),
            "{requests:?}"
        );
    }

    #[test]
    fn hides_the_api_key_in_what_fair_witness_prints() {
        let output = Cursor::new("{\"error\":\"sk-secret is not a key\"}\nsk-secre\n");

        let read: Vec<String> = lines(output, Some("sk-secret".to_string()))
            .iter()
            .collect();

        assert_eq!(read, ["{\"error\":\"[API key] is not a key\"}", "sk-secre"]);
    }
}
