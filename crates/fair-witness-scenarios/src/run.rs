use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fair_witness_core::session::{Event, Record};
use fair_witness_core::testing::fake_model;

use crate::scenario::{DMP, FILES, SCRIPT, Scenario};

/// Generous, so that a slow CI machine still makes it, while a scenario that hangs fails.
const STARTUP: Duration = Duration::from_secs(20);
const REVIEW: Duration = Duration::from_secs(20);
const SCRIPT_STEP: Duration = Duration::from_secs(60);
const STOP: Duration = Duration::from_secs(10);

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
}

/// What a run of a scenario produced: every event fair-witness printed and, when the run went
/// wrong or stopped early, why.
pub struct Ran {
    pub records: Vec<Record>,
    pub error: Option<String>,
}

/// Copies the scenario's files into a new folder, watches it with headless fair-witness and the
/// fake model, and runs the script, which stops after each change set until it is reviewed.
pub fn run(scenario: &Scenario, settings: &Settings) -> Ran {
    let mut records = Vec::new();
    let error = run_into(scenario, settings, &mut records)
        .err()
        .map(|e| format!("{e:#}"));
    Ran { records, error }
}

fn run_into(scenario: &Scenario, settings: &Settings, records: &mut Vec<Record>) -> Result<()> {
    let folder = tempfile::Builder::new()
        .prefix("fair-witness-scenario-")
        .tempdir()
        .context("could not create the project folder")?;
    let files = scenario.dir.join(FILES);
    if files.is_dir() {
        copy_dir(&files, folder.path())?;
    }
    let (endpoint, _model) = fake_model(scenario.fake_replies());
    let mut witness = Process::start(
        "fair-witness",
        Command::new(&settings.binary)
            .args(["run", "--headless", "--project"])
            .arg(folder.path())
            .arg("--dmp-file")
            .arg(scenario.dir.join(DMP))
            .args(["--ai-url", &endpoint, "--model", "fake"])
            .env(
                "FAIR_WITNESS_DEBOUNCE_MS",
                settings.debounce.as_millis().to_string(),
            )
            // A key meant for a real model must not go to the fake one.
            .env_remove("FAIR_WITNESS_API_KEY"),
    )
    .with_context(|| format!("could not start {}", settings.binary.display()))?;
    witness.until_watching()?;

    let mut script = Process::start_group(
        SCRIPT,
        Command::new("bash")
            .arg("-c")
            .arg(prelude())
            .arg(scenario.dir.join(SCRIPT))
            .current_dir(folder.path()),
    )
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
                witness.until_decided(set, records)?;
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
    // Changes the script made after it last waited come once they are older than the debounce,
    // which the watcher checks every quarter of it.
    witness.collect_for(
        settings.debounce * 3 / 2 + Duration::from_millis(200),
        records,
    )?;
    witness.stop(records)
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
    fn start(name: &'static str, command: &mut Command) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Self {
            name,
            group: None,
            stdin: child.stdin.take(),
            stdout: lines(child.stdout.take().expect("stdout is piped")),
            stderr: lines(child.stderr.take().expect("stderr is piped")),
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
        let mut process = Self::start(name, command)?;
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

    /// Reads events until change set `set` has its verdict or its review failed.
    fn until_decided(&mut self, set: usize, records: &mut Vec<Record>) -> Result<()> {
        let deadline = Instant::now() + REVIEW;
        let mut changed = false;
        loop {
            let record = match self.next_record(deadline)? {
                Some(record) => record,
                None if changed => bail!("change set {set} got no verdict within {REVIEW:?}"),
                None => bail!(
                    "fair-witness saw no changes before {SCRIPT} waited for verdict {set}, \
                     within {REVIEW:?}"
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

fn lines(output: impl Read + Send + 'static) -> Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                return;
            }
        }
    });
    receiver
}

fn left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}
