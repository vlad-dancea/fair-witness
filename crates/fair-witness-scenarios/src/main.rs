//! Runs the researcher scenarios in `scenarios/` against headless fair-witness with the fake
//! model, prints how each went and writes the results and events as JSON.

mod report;
mod run;
mod scenario;

use std::env;
use std::fs;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use fair_witness_core::session::Record;
use jiff::Timestamp;
use serde::Serialize;

use report::Outcome;
use run::Settings;
use scenario::Scenario;

/// Each run holds an inotify instance, and Linux allows a user 128 by default, which editors and
/// other tools use too.
const MOST_JOBS: usize = 8;

#[derive(Parser)]
#[command(about = "Run the researcher scenarios against headless fair-witness with a fake model")]
struct Args {
    #[arg(help = "Run only the scenarios whose names contain one of these")]
    names: Vec<String>,
    #[arg(
        long,
        value_name = "DIR",
        default_value = "scenarios",
        help = "The folder that holds the scenarios"
    )]
    scenarios: PathBuf,
    #[arg(
        long,
        value_name = "PATH",
        help = "The fair-witness binary to run, instead of the one built next to this one"
    )]
    binary: Option<PathBuf>,
    #[arg(
        long,
        value_name = "N",
        help = "How many scenarios run at once [default: the number of CPUs, at most 8]"
    )]
    jobs: Option<NonZeroUsize>,
    #[arg(
        long,
        value_name = "MS",
        default_value_t = 500,
        help = "How old a change must be to go into a change set, as FAIR_WITNESS_DEBOUNCE_MS"
    )]
    debounce_ms: u64,
    #[arg(
        long,
        value_name = "DIR",
        default_value = "target/scenarios",
        help = "Where to write the results, in a new folder for each run"
    )]
    output: PathBuf,
}

/// What a run of all the scenarios writes to `results.json`.
#[derive(Serialize)]
struct Results<'a> {
    started: Timestamp,
    model: &'a str,
    debounce_ms: u64,
    scenarios: &'a [Outcome],
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Runs the chosen scenarios and says whether all of them passed.
fn run() -> Result<bool> {
    let args = Args::parse();
    #[cfg(unix)]
    stop_scripts_on_signal()?;
    let binary = match args.binary {
        Some(binary) => binary,
        None => built_binary()?,
    };
    let dirs: Vec<PathBuf> = scenario::find(&args.scenarios)?
        .into_iter()
        .filter(|dir| {
            let name = dir.file_name().unwrap_or_default().to_string_lossy();
            args.names.is_empty() || args.names.iter().any(|part| name.contains(part.as_str()))
        })
        .collect();
    if dirs.is_empty() {
        let only = if args.names.is_empty() {
            String::new()
        } else {
            format!(" whose names contain {}", args.names.join(" or "))
        };
        bail!("found no scenarios in {}{only}", args.scenarios.display());
    }
    let settings = Settings {
        binary,
        debounce: Duration::from_millis(args.debounce_ms),
    };
    let jobs = args.jobs.map_or_else(
        || {
            thread::available_parallelism()
                .map_or(1, usize::from)
                .min(MOST_JOBS)
        },
        usize::from,
    );

    let started = Timestamp::now();
    let (outcomes, events): (Vec<Outcome>, Vec<Vec<Record>>) =
        run_all(&dirs, &settings, jobs).into_iter().unzip();
    let folder = new_folder(
        &args.output,
        &started.strftime("%Y%m%dT%H%M%SZ").to_string(),
    )?;
    let results = Results {
        started,
        model: "fake",
        debounce_ms: args.debounce_ms,
        scenarios: &outcomes,
    };
    print_table(&outcomes);
    write_results(&folder, &results, &events)?;
    let passed = outcomes.iter().filter(|outcome| outcome.passed).count();
    println!(
        "\n{passed} of {} scenarios passed. Results and events are in {}",
        outcomes.len(),
        folder.display()
    );
    Ok(passed == outcomes.len())
}

/// Stops the scripts that are running when the runner is stopped, since they run in process
/// groups of their own, which Ctrl+C in the terminal does not reach.
#[cfg(unix)]
fn stop_scripts_on_signal() -> Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

    let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP])
        .context("could not listen for signals")?;
    thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            run::stop_scripts();
            std::process::exit(128 + signal);
        }
    });
    Ok(())
}

/// fair-witness as cargo builds it, next to this binary.
fn built_binary() -> Result<PathBuf> {
    let binary = env::current_exe()
        .context("could not find this binary")?
        .with_file_name(format!("fair-witness{}", env::consts::EXE_SUFFIX));
    if !binary.is_file() {
        bail!(
            "could not find {}; build it with `cargo build -p fair-witness` or pass --binary",
            binary.display()
        );
    }
    Ok(binary)
}

/// Runs the scenarios in `dirs`, `jobs` at a time, and returns how each went with its events,
/// in the order of `dirs`.
fn run_all(dirs: &[PathBuf], settings: &Settings, jobs: usize) -> Vec<(Outcome, Vec<Record>)> {
    let next = AtomicUsize::new(0);
    let done = Mutex::new(Vec::new());
    thread::scope(|scope| {
        for _ in 0..jobs.min(dirs.len()) {
            scope.spawn(|| {
                while let Some(dir) = dirs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let result = run_one(dir, settings);
                    done.lock().unwrap().push((dir, result));
                }
            });
        }
    });
    let mut done = done.into_inner().unwrap();
    done.sort_by_key(|(dir, _)| *dir);
    done.into_iter().map(|(_, result)| result).collect()
}

fn run_one(dir: &Path, settings: &Settings) -> (Outcome, Vec<Record>) {
    match Scenario::load(dir) {
        Ok(scenario) => {
            let ran = run::run(&scenario, settings);
            (report::judge(&scenario, &ran), ran.records)
        }
        Err(e) => {
            let name = dir.file_name().unwrap_or_default().to_string_lossy();
            (
                Outcome::invalid(name.into_owned(), format!("{e:#}")),
                Vec::new(),
            )
        }
    }
}

/// Creates the folder `name` in `output`, or `name-2` and so on when another run that started
/// in the same second has it.
fn new_folder(output: &Path, name: &str) -> Result<PathBuf> {
    fs::create_dir_all(output).with_context(|| format!("could not create {}", output.display()))?;
    for number in 1.. {
        let folder = match number {
            1 => output.join(name),
            _ => output.join(format!("{name}-{number}")),
        };
        match fs::create_dir(&folder) {
            Ok(()) => return Ok(folder),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(e).with_context(|| format!("could not create {}", folder.display()));
            }
        }
    }
    unreachable!("there is always a number left")
}

/// Writes `results.json` and the events of each scenario, one per line as fair-witness printed
/// them, to `<scenario>.jsonl` in `folder`.
fn write_results(folder: &Path, results: &Results, events: &[Vec<Record>]) -> Result<()> {
    let path = folder.join("results.json");
    fs::write(&path, serde_json::to_string_pretty(results)? + "\n")
        .with_context(|| format!("could not write {}", path.display()))?;
    for (outcome, records) in results.scenarios.iter().zip(events) {
        let mut lines = String::new();
        for record in records {
            lines.push_str(&serde_json::to_string(record)?);
            lines.push('\n');
        }
        let path = folder.join(format!("{}.jsonl", outcome.name));
        fs::write(&path, lines).with_context(|| format!("could not write {}", path.display()))?;
    }
    Ok(())
}

fn print_table(outcomes: &[Outcome]) {
    let width = outcomes
        .iter()
        .map(|outcome| outcome.name.len())
        .chain(["scenario".len()])
        .max()
        .unwrap_or_default();
    println!("{:<6}{:<width$}  sets passed", "", "scenario");
    for outcome in outcomes {
        let result = if outcome.passed { "pass" } else { "FAIL" };
        println!(
            "{result:<6}{:<width$}  {} of {}",
            outcome.name,
            outcome.sets_passed(),
            outcome.sets.len()
        );
    }
    for outcome in outcomes.iter().filter(|outcome| !outcome.passed) {
        println!("\n{}:", outcome.name);
        for problem in &outcome.problems {
            println!("  {}", indent(problem));
        }
        for (number, set) in (1..).zip(&outcome.sets) {
            for problem in &set.problems {
                println!("  set {number}: {}", indent(problem));
            }
        }
    }
}

/// Indents the lines after the first of a problem, such as the output of a failed script, under
/// it.
fn indent(problem: &str) -> String {
    let lines: Vec<&str> = problem.trim_end().lines().map(str::trim_end).collect();
    lines.join("\n    ")
}
