//! Runs the researcher scenarios in `scenarios/` against headless fair-witness, with the fake
//! model or real ones, prints how each went and writes the results and events as JSON.

#[cfg(test)]
mod fixtures;
mod report;
mod run;
mod scenario;
mod tables;

use std::collections::HashSet;
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
use fair_witness_core::project::Effort;
use fair_witness_core::session::Record;
use jiff::Timestamp;
use serde::Serialize;
use url::{Host, Url};

use report::{Accuracy, Outcome};
use run::{API_KEY, Model, Settings, can_hide};
use scenario::Scenario;

/// Each run holds an inotify instance, and Linux allows a user 128 by default, which editors and
/// other tools use too.
const MOST_JOBS: usize = 8;
/// A real model's verdicts vary, so each scenario runs this often with each by default.
const REPEATS: usize = 5;
/// TU Wien's AI gateway, which `fair-witness setup` offers first too.
const AQUEDUCT: &str = "https://aqueduct.ai.datalab.tuwien.ac.at/v1";

#[derive(Parser)]
#[command(
    about = "Run the researcher scenarios against headless fair-witness, with a fake model or \
             real ones",
    after_help = "With --model, the scenarios run against real models, each at every --effort, \
                  and the runner reports how often each gave the expected verdict. The API key \
                  comes from FAIR_WITNESS_API_KEY, which only fair-witness gets."
)]
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
    #[arg(
        long = "model",
        value_name = "ID",
        help = "Review with this model of the API instead of the fake one; repeat for more"
    )]
    models: Vec<String>,
    #[arg(
        long = "effort",
        value_name = "EFFORT",
        value_enum,
        requires = "models",
        help = "Run each model at this reasoning effort; repeat for more [default: default]"
    )]
    efforts: Vec<EffortArg>,
    #[arg(
        long,
        value_name = "URL",
        default_value = AQUEDUCT,
        requires = "models",
        help = "The OpenAI-compatible API of the models"
    )]
    ai_url: String,
    #[arg(
        long,
        value_name = "N",
        help = "How often each scenario runs with each model and effort [default: 5 with \
                --model, 1 with the fake model]"
    )]
    repeats: Option<NonZeroUsize>,
}

/// A reasoning effort as `fair-witness run --effort` takes it, where `default` leaves it to the
/// model.
#[derive(Clone, Copy, clap::ValueEnum)]
enum EffortArg {
    Default,
    None,
    Low,
    Medium,
    High,
}

impl EffortArg {
    fn effort(self) -> Option<Effort> {
        match self {
            EffortArg::Default => None,
            EffortArg::None => Some(Effort::None),
            EffortArg::Low => Some(Effort::Low),
            EffortArg::Medium => Some(Effort::Medium),
            EffortArg::High => Some(Effort::High),
        }
    }
}

/// A run of a scenario with a model, the how-manyth with it.
struct Job<'a> {
    scenario: &'a Scenario,
    model: &'a Model,
    repeat: usize,
    /// Where its outcome goes among the others, which are by model, then scenario, then repeat.
    place: usize,
}

/// What a run of all the scenarios writes to `results.json`.
#[derive(Serialize)]
struct Results<'a> {
    started: Timestamp,
    /// The API of the real models.
    #[serde(skip_serializing_if = "Option::is_none")]
    ai_url: Option<&'a str>,
    repeats: usize,
    debounce_ms: u64,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    accuracy: &'a [Accuracy],
    runs: &'a [Outcome],
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

/// Runs the chosen scenarios and says whether all of them went as expected.
fn run() -> Result<bool> {
    let args = Args::parse();
    #[cfg(unix)]
    stop_scripts_on_signal()?;
    let binary = match &args.binary {
        Some(binary) => binary.clone(),
        None => built_binary()?,
    };
    let scenarios = load(&args.scenarios, &args.names)?;
    let models = models(&args)?;
    let fake = models == [Model::Fake];
    let repeats = args
        .repeats
        .map_or(if fake { 1 } else { REPEATS }, usize::from);
    let api_key = if fake {
        None
    } else {
        env::var(API_KEY).ok().filter(|key| !key.is_empty())
    };
    if !fake && api_key.is_none() {
        eprintln!("{API_KEY} is not set, so the models are asked without an API key.");
    }
    if api_key.is_some() && in_the_clear(&args.ai_url) {
        bail!(
            "--ai-url uses plain HTTP to another machine, which would send {API_KEY} \
             unencrypted. Use https://, or http:// only for this machine, such as for a stub API"
        );
    }
    if api_key.as_deref().is_some_and(|key| !can_hide(key)) {
        bail!(
            "{API_KEY} may hold only printable ASCII characters other than a backslash, or the \
             runner could not hide it in its results"
        );
    }
    let settings = Settings {
        binary,
        debounce: Duration::from_millis(args.debounce_ms),
        api_key,
    };
    let mut jobs = Vec::new();
    for repeat in 1..=repeats {
        for (scenario_index, scenario) in scenarios.iter().enumerate() {
            for (model_index, model) in taking_turns(&models) {
                jobs.push(Job {
                    scenario,
                    model,
                    repeat,
                    place: (model_index * scenarios.len() + scenario_index) * repeats + repeat - 1,
                });
            }
        }
    }
    let at_once = args.jobs.map_or_else(
        || {
            thread::available_parallelism()
                .map_or(1, usize::from)
                .min(MOST_JOBS)
        },
        usize::from,
    );

    let started = Timestamp::now();
    let (outcomes, events): (Vec<Outcome>, Vec<Vec<Record>>) =
        run_all(&jobs, &settings, at_once, !fake)
            .into_iter()
            .unzip();
    let folder = new_folder(
        &args.output,
        &started.strftime("%Y%m%dT%H%M%SZ").to_string(),
    )?;
    let accuracy = if fake {
        Vec::new()
    } else {
        report::accuracy(&outcomes)
    };
    let results = Results {
        started,
        ai_url: (!fake).then_some(args.ai_url.as_str()),
        repeats,
        debounce_ms: args.debounce_ms,
        accuracy: &accuracy,
        runs: &outcomes,
    };
    let one_each = jobs.len() == scenarios.len();
    if fake {
        print!("{}", tables::passed(&outcomes, one_each));
    } else {
        print!("{}", tables::accuracy(&outcomes, &accuracy));
    }
    print!("{}", tables::problems(&outcomes, fake, one_each));
    write_results(&folder, &results, &events)?;
    let passed = outcomes.iter().filter(|outcome| outcome.passed).count();
    let runs = if one_each { "scenarios" } else { "runs" };
    let went = if fake {
        "passed"
    } else {
        "ran without problems"
    };
    println!(
        "\n{passed} of {} {runs} {went}. Results and events are in {}",
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

/// The scenarios in `root` whose names contain one of `names`, or all of them without names.
/// One that cannot be read stops the runner before anything runs.
fn load(root: &Path, names: &[String]) -> Result<Vec<Scenario>> {
    let dirs: Vec<PathBuf> = scenario::find(root)?
        .into_iter()
        .filter(|dir| {
            let name = dir.file_name().unwrap_or_default().to_string_lossy();
            names.is_empty() || names.iter().any(|part| name.contains(part.as_str()))
        })
        .collect();
    if dirs.is_empty() {
        let only = if names.is_empty() {
            String::new()
        } else {
            format!(" whose names contain {}", names.join(" or "))
        };
        bail!("found no scenarios in {}{only}", root.display());
    }
    let mut scenarios = Vec::new();
    let mut errors = Vec::new();
    for dir in dirs {
        match Scenario::load(&dir) {
            Ok(scenario) => scenarios.push(scenario),
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }
    Ok(scenarios)
}

/// The models to review with: the fake one, or each `--model` at each `--effort`.
fn models(args: &Args) -> Result<Vec<Model>> {
    if args.models.is_empty() {
        return Ok(vec![Model::Fake]);
    }
    let efforts: Vec<Option<Effort>> = if args.efforts.is_empty() {
        vec![None]
    } else {
        args.efforts.iter().map(|effort| effort.effort()).collect()
    };
    let mut models = Vec::new();
    let mut folders = HashSet::new();
    for name in &args.models {
        for &effort in &efforts {
            let model = Model::Real {
                url: args.ai_url.clone(),
                name: name.clone(),
                effort,
            };
            if !folders.insert(model.folder()) {
                bail!(
                    "two of the models would write their events to {}; name each model and \
                     effort once",
                    model.folder()
                );
            }
            models.push(model);
        }
    }
    Ok(models)
}

/// The models with their places in `models`, in the order their runs take turns: each model at
/// its first effort, then each at its second and so on, since an API limits how often each model
/// may be asked.
fn taking_turns(models: &[Model]) -> Vec<(usize, &Model)> {
    let mut order: Vec<(usize, usize, &Model)> = models
        .iter()
        .enumerate()
        .map(|(index, model)| {
            let before = models[..index]
                .iter()
                .filter(|other| other.name() == model.name())
                .count();
            (before, index, model)
        })
        .collect();
    order.sort_by_key(|(before, index, _)| (*before, *index));
    order
        .into_iter()
        .map(|(_, index, model)| (index, model))
        .collect()
}

/// Runs `jobs` in their order, `at_once` at a time, and returns how each went with its events, in
/// the order of their places. With `progress`, says on stderr when each one ends, since a real
/// model takes a while.
fn run_all(
    jobs: &[Job],
    settings: &Settings,
    at_once: usize,
    progress: bool,
) -> Vec<(Outcome, Vec<Record>)> {
    let next = AtomicUsize::new(0);
    let done = Mutex::new(Vec::new());
    thread::scope(|scope| {
        for _ in 0..at_once.min(jobs.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else {
                        return;
                    };
                    let ran = run::run(job.scenario, job.model, settings);
                    let outcome = report::judge(job.scenario, job.model, job.repeat, &ran);
                    let mut done = done.lock().unwrap();
                    if progress {
                        eprintln!(
                            "{}/{} {}: {} of {} right{}",
                            done.len() + 1,
                            jobs.len(),
                            tables::label(&outcome, false, false),
                            outcome.sets_correct(),
                            outcome.sets.len(),
                            if outcome.passed {
                                ""
                            } else {
                                ", with problems"
                            }
                        );
                    }
                    done.push((job.place, (outcome, ran.records)));
                }
            });
        }
    });
    let mut done = done.into_inner().unwrap();
    done.sort_by_key(|(place, _)| *place);
    done.into_iter().map(|(_, result)| result).collect()
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

/// Writes `results.json` and the events of each run, one per line as fair-witness printed them,
/// to the file its outcome names in `folder`.
fn write_results(folder: &Path, results: &Results, events: &[Vec<Record>]) -> Result<()> {
    let path = folder.join("results.json");
    fs::write(&path, serde_json::to_string_pretty(results)? + "\n")
        .with_context(|| format!("could not write {}", path.display()))?;
    for (outcome, records) in results.runs.iter().zip(events) {
        let mut lines = String::new();
        for record in records {
            lines.push_str(&serde_json::to_string(record)?);
            lines.push('\n');
        }
        let path = folder.join(&outcome.events);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        fs::write(&path, lines).with_context(|| format!("could not write {}", path.display()))?;
    }
    Ok(())
}

/// Whether a key sent to `ai_url` would cross the network unencrypted: plain HTTP to another
/// machine than this one. fair-witness reads an address without a scheme as HTTPS.
fn in_the_clear(ai_url: &str) -> bool {
    let Ok(url) = Url::parse(ai_url.trim()) else {
        return false;
    };
    url.scheme() == "http"
        && !match url.host() {
            Some(Host::Domain(domain)) => domain == "localhost",
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_turns_between_the_models_before_the_efforts() {
        let model = |name: &str, effort| Model::Real {
            url: AQUEDUCT.to_string(),
            name: name.to_string(),
            effort,
        };
        let models = [
            model("large", None),
            model("large", Some(Effort::Low)),
            model("small", None),
            model("small", Some(Effort::Low)),
        ];

        let order: Vec<usize> = taking_turns(&models)
            .into_iter()
            .map(|(index, _)| index)
            .collect();

        assert_eq!(order, [0, 2, 1, 3]);
    }

    #[test]
    fn sends_a_key_over_plain_http_only_to_this_machine() {
        assert_eq!(
            [
                AQUEDUCT,
                "models.example.org/v1",
                "http://127.0.0.1:8080/v1",
                "http://LOCALHOST:1234",
                "http://[::1]:9/v1",
                "http://models.example.org/v1",
                "http://10.0.0.5:8080/v1",
                "HTTP://Models.Example.org",
                "http://127.0.0.1@models.example.org/v1",
                "http://models.example.org\\@127.0.0.1/v1",
                "http://localhost.example.org",
            ]
            .map(in_the_clear),
            [
                false, false, false, false, false, true, true, true, true, true, true
            ]
        );
    }
}
