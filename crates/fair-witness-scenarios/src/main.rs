//! Runs the researcher scenarios in `scenarios/` against headless fair-witness, with the fake
//! model or real ones, prints how each went and writes the results and events as JSON as each
//! run ends.

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
use std::mem;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use fair_witness_core::net::{in_the_clear, web_url};
use fair_witness_core::project::Effort;
use fair_witness_core::secret::can_hide;
use fair_witness_core::session::Record;
use jiff::Timestamp;
use serde::Serialize;

use report::{Accuracy, Refused, RunOutcome};
use run::{API_KEY, Model, Settings};
use scenario::{Kind, Scenario};

/// Each run holds an inotify instance, and Linux allows a user 128 by default, which editors and
/// other tools use too.
const MOST_JOBS: usize = 8;
/// A real model's verdicts vary, so each scenario runs this often with each by default.
const REPEATS: usize = 5;
/// TU Wien's AI gateway, which `fair-witness setup` offers first too.
const AQUEDUCT: &str = "https://aqueduct.ai.datalab.tuwien.ac.at/v1";

/// How many runs that ended before a signal stopped the scripts are not saved yet, which the
/// signal waits for.
static UNSAVED: Mutex<usize> = Mutex::new(0);
static SAVED: Condvar = Condvar::new();

#[derive(Parser)]
#[command(
    about = "Run the researcher scenarios against headless fair-witness, with a fake model or \
             real ones",
    after_help = "The scenarios are in two folders: edge-cases/, which run only with the fake \
                  model, and realistic/. With --model, the realistic scenarios run against real \
                  models, each at every --effort, and the runner reports how often each gave the \
                  expected verdict. It first asks the API once about each model and effort, and \
                  one the API refuses does not run. The API key comes from \
                  FAIR_WITNESS_API_KEY, which only these checks and fair-witness get."
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
        help = "How long the folder must be quiet before its changes count as one set in the \
                edge cases, as FAIR_WITNESS_DEBOUNCE_MS. Realistic scenarios run at \
                fair-witness's own"
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
        help = "Review the realistic scenarios with this model of the API instead of the fake one; \
                repeat for more"
    )]
    models: Vec<String>,
    #[arg(
        long = "effort",
        value_name = "EFFORT",
        requires = "models",
        help = "Run each model at this reasoning effort, by a name it takes such as low or \
                xhigh; repeat for more. `default` sends no effort, so the model uses its own, \
                and `none` asks for no reasoning, which some models ignore [default: default]"
    )]
    efforts: Vec<Effort>,
    #[arg(
        long,
        value_name = "URL",
        default_value = AQUEDUCT,
        value_parser = web_url,
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
struct Results {
    started: Timestamp,
    status: Status,
    /// The API of the real models.
    #[serde(skip_serializing_if = "Option::is_none")]
    ai_url: Option<String>,
    repeats: usize,
    debounce_ms: Debounces,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    accuracy: Vec<Accuracy>,
    /// The models and efforts the API refused, which did not run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    refused: Vec<Refused>,
    /// The runs that have ended.
    runs: Vec<RunOutcome>,
}

/// How far the runner got. A runner that crashed or was killed leaves `running` behind.
#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Running,
    /// A signal stopped the runner, so the runs that had not ended are missing.
    Interrupted,
    Finished,
}

/// How long fair-witness waited for the folder to be quiet in each kind of scenario.
#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct Debounces {
    edge_cases: u64,
    realistic: u64,
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
    let binary = match &args.binary {
        Some(binary) => binary.clone(),
        None => built_binary()?,
    };
    let mut models = models(&args)?;
    let fake = models == [Model::Fake];
    let scenarios = load(&args.scenarios, &args.names, fake)?;
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
    let refused = run::check(&models, &settings)?;
    let tried = models.len();
    models.retain(|model| {
        !refused
            .iter()
            .any(|refused| refused.model == model.name() && refused.effort == model.effort())
    });
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

    let results = Results {
        started: Timestamp::now(),
        status: Status::Running,
        ai_url: (!fake).then(|| args.ai_url.clone()),
        repeats,
        debounce_ms: Debounces {
            edge_cases: args.debounce_ms,
            realistic: u64::try_from(fair_witness_core::watcher::DEBOUNCE.as_millis())
                .unwrap_or(u64::MAX),
        },
        accuracy: Vec::new(),
        refused,
        runs: Vec::new(),
    };
    let output = Output::create(&args.output, results)?;
    if !fake {
        eprintln!(
            "The results go to {} as the runs end.",
            output.folder.display()
        );
    }
    let output = Arc::new(Mutex::new(output));
    #[cfg(unix)]
    save_on_signal(Arc::clone(&output))?;
    let ran = run_all(&jobs, &settings, at_once, !fake, &output);
    if run::stopping() {
        // The signal ends the runner, with its exit code, once the runs that ended are saved.
        loop {
            thread::park();
        }
    }
    ran?;
    let mut output = output.lock().unwrap_or_else(PoisonError::into_inner);
    output.end(Status::Finished)?;
    // Taken, so that a signal does not wait for the report while stdout blocks. Nothing
    // writes finished results again.
    let folder = output.folder.clone();
    let outcomes = mem::take(&mut output.results.runs);
    let accuracy = mem::take(&mut output.results.accuracy);
    let refused = mem::take(&mut output.results.refused);
    drop(output);
    let one_each = jobs.len() == scenarios.len();
    if fake {
        print!("{}", tables::passed(&outcomes, one_each));
    } else if !outcomes.is_empty() {
        print!("{}", tables::accuracy(&outcomes, &accuracy));
    }
    print!("{}", tables::refused(&refused));
    print!("{}", tables::problems(&outcomes, fake, one_each));
    let passed = outcomes.iter().filter(|outcome| outcome.passed).count();
    let runs = if one_each { "scenarios" } else { "runs" };
    let went = if fake {
        "passed"
    } else {
        "ran without problems"
    };
    let refusals = match refused.len() {
        0 => String::new(),
        count => format!(", and the API refused {count} of {tried} models and efforts"),
    };
    println!(
        "\n{passed} of {} {runs} {went}{refusals}. Results and events are in {}",
        outcomes.len(),
        folder.display()
    );
    Ok(passed == outcomes.len() && refused.is_empty())
}

/// When the runner is stopped, stops the scripts that are running, since they run in process
/// groups of their own, which Ctrl+C in the terminal does not reach, and marks the results of
/// the runs that ended as interrupted.
#[cfg(unix)]
fn save_on_signal(output: Arc<Mutex<Output>>) -> Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

    let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP])
        .context("could not listen for signals")?;
    thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            // Before anything waits for the output, so that a disk that stalls cannot keep the
            // scripts running. The runs they were in are not saved, as `count_unsaved` says.
            run::stop_scripts();
            drop(
                SAVED
                    .wait_while(
                        UNSAVED.lock().unwrap_or_else(PoisonError::into_inner),
                        |unsaved| *unsaved > 0,
                    )
                    .unwrap_or_else(PoisonError::into_inner),
            );
            // Held until the exit, so that the runner does not mark its results finished.
            let mut output = output.lock().unwrap_or_else(PoisonError::into_inner);
            // Says nothing unless saving failed, and then waits at most a second for stderr,
            // since a stderr that blocks would keep the runner from exiting.
            if output.results.status == Status::Running
                && let Err(e) = output.end(Status::Interrupted)
            {
                let (said, heard) = std::sync::mpsc::channel();
                thread::spawn(move || {
                    eprintln!("\nerror: {e:#}");
                    let _ = said.send(());
                });
                let _ = heard.recv_timeout(Duration::from_secs(1));
            }
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

/// The scenarios in `root` whose names contain one of `names`, or all of them without names,
/// and only the realistic ones unless the model is the `fake` one, since the edge cases measure
/// nothing. One that cannot be read stops the runner before anything runs.
fn load(root: &Path, names: &[String], fake: bool) -> Result<Vec<Scenario>> {
    let found = scenario::find(root)?;
    let mut seen = HashSet::new();
    for (_, dir) in &found {
        let name = dir.file_name().unwrap_or_default();
        if !seen.insert(name) {
            bail!(
                "two scenarios in {} are called {}; rename one",
                root.display(),
                name.to_string_lossy()
            );
        }
    }
    let named: Vec<(Kind, PathBuf)> = found
        .into_iter()
        .filter(|(_, dir)| {
            let name = dir.file_name().unwrap_or_default().to_string_lossy();
            names.is_empty() || names.iter().any(|part| name.contains(part.as_str()))
        })
        .collect();
    let only = if names.is_empty() {
        String::new()
    } else {
        format!(" whose names contain {}", names.join(" or "))
    };
    if named.is_empty() {
        bail!(
            "found no scenarios in {}/{} or {}/{}{only}",
            root.display(),
            Kind::EdgeCase.folder(),
            root.display(),
            Kind::Realistic.folder()
        );
    }
    let chosen: Vec<(Kind, PathBuf)> = named
        .into_iter()
        .filter(|(kind, _)| fake || *kind == Kind::Realistic)
        .collect();
    if chosen.is_empty() {
        bail!(
            "found only edge cases{only}, which run only with the fake model; real models run \
             the scenarios in {}/{}",
            root.display(),
            Kind::Realistic.folder()
        );
    }
    let mut scenarios = Vec::new();
    let mut errors = Vec::new();
    for (kind, dir) in chosen {
        match Scenario::load(kind, &dir) {
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
    let efforts = if args.efforts.is_empty() {
        vec![Effort::default()]
    } else {
        args.efforts.clone()
    };
    let mut models = Vec::new();
    let mut folders = HashSet::new();
    for name in &args.models {
        for effort in &efforts {
            let model = Model::Real {
                url: args.ai_url.clone(),
                name: name.clone(),
                effort: effort.clone(),
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

/// Runs `jobs` in their order, `at_once` at a time, and saves how each went with its events in
/// `output` as it ends. With `progress`, says on stderr when each one ends, since a real model
/// takes a while. Once a run cannot be saved, no other starts.
fn run_all(
    jobs: &[Job],
    settings: &Settings,
    at_once: usize,
    progress: bool,
    output: &Mutex<Output>,
) -> Result<()> {
    let next = AtomicUsize::new(0);
    let ended = AtomicUsize::new(0);
    let failed = Mutex::new(None);
    thread::scope(|scope| {
        for _ in 0..at_once.min(jobs.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else {
                        return;
                    };
                    let ran = run::run(job.scenario, job.model, settings);
                    if !count_unsaved() {
                        return;
                    }
                    let outcome = report::judge(job.scenario, job.model, job.repeat, &ran);
                    let how = progress.then(|| {
                        format!(
                            "{}: {} of {} right{}",
                            tables::label(&outcome, false, false),
                            outcome.sets_correct(),
                            outcome.sets.len(),
                            if outcome.passed {
                                ""
                            } else {
                                ", with problems"
                            }
                        )
                    });
                    let saved = output.lock().unwrap_or_else(PoisonError::into_inner).save(
                        job.place,
                        outcome,
                        &ran.records,
                    );
                    *UNSAVED.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
                    SAVED.notify_all();
                    if let Err(e) = saved {
                        next.store(jobs.len(), Ordering::Relaxed);
                        failed.lock().unwrap().get_or_insert(e);
                    }
                    // Said once the run is saved and the output unlocked, so that a stderr that
                    // blocks neither keeps the run from being saved nor holds up a signal.
                    if let Some(how) = how {
                        let ended = ended.fetch_add(1, Ordering::Relaxed) + 1;
                        eprintln!("{ended}/{} {how}", jobs.len());
                    }
                }
            });
        }
    });
    match failed.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Counts a run that has just ended as unsaved, unless a signal has stopped the scripts, which
/// may have cut it short.
fn count_unsaved() -> bool {
    let mut unsaved = UNSAVED.lock().unwrap_or_else(PoisonError::into_inner);
    if run::stopping() {
        return false;
    }
    *unsaved += 1;
    true
}

/// The folder of a run of the runner, whose `results.json` holds the runs that have ended at
/// any time, so that an interrupted or crashed runner keeps them.
struct Output {
    folder: PathBuf,
    results: Results,
    /// The place of each run in `results.runs`, which keeps them in the order of their places.
    places: Vec<usize>,
}

impl Output {
    /// Creates a new folder in `output`, named after when the runner started, with
    /// `results.json` in it.
    fn create(output: &Path, results: Results) -> Result<Self> {
        let name = results.started.strftime("%Y%m%dT%H%M%SZ").to_string();
        let output = Self {
            folder: new_folder(output, &name)?,
            results,
            places: Vec::new(),
        };
        output.write()?;
        Ok(output)
    }

    /// Writes the events of the run at `place`, one per line as fair-witness printed them, to
    /// the file its outcome names, and adds the outcome to `results.json`.
    fn save(&mut self, place: usize, outcome: RunOutcome, records: &[Record]) -> Result<()> {
        let mut lines = String::new();
        for record in records {
            lines.push_str(&serde_json::to_string(record)?);
            lines.push('\n');
        }
        let path = self.folder.join(&outcome.events);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        fs::write(&path, lines).with_context(|| format!("could not write {}", path.display()))?;
        let index = self.places.partition_point(|before| *before < place);
        self.places.insert(index, place);
        self.results.runs.insert(index, outcome);
        // Only real models have an API, and only they are measured.
        if self.results.ai_url.is_some() {
            self.results.accuracy = report::accuracy(&self.results.runs);
        }
        self.write()
    }

    fn end(&mut self, status: Status) -> Result<()> {
        self.results.status = status;
        self.write()
    }

    /// Replaces `results.json` in one step, so that a runner stopped while writing it leaves the
    /// one before.
    fn write(&self) -> Result<()> {
        let path = self.folder.join("results.json");
        let part = self.folder.join("results.json.part");
        fs::write(&part, serde_json::to_string_pretty(&self.results)? + "\n")
            .with_context(|| format!("could not write {}", part.display()))?;
        fs::rename(&part, &path).with_context(|| format!("could not write {}", path.display()))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of scenarios with an edge case and a realistic scenario of each name.
    fn scenarios(kinds: &[(Kind, &str)]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for (kind, name) in kinds {
            let dir = root.path().join(kind.folder()).join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("scenario.toml"),
                "description = \"\"\n[[set]]\ncontradicts = false\nexplanation = \"\"\n",
            )
            .unwrap();
            fs::write(dir.join("dmp.json"), "{}").unwrap();
            fs::write(dir.join("script.sh"), "").unwrap();
        }
        root
    }

    fn names(scenarios: &[Scenario]) -> Vec<(Kind, &str)> {
        scenarios
            .iter()
            .map(|scenario| (scenario.kind, scenario.name.as_str()))
            .collect()
    }

    #[test]
    fn runs_the_edge_cases_only_with_the_fake_model() {
        let root = scenarios(&[
            (Kind::Realistic, "notes"),
            (Kind::EdgeCase, "odd-names"),
            (Kind::EdgeCase, "many-files"),
        ]);

        let fake = load(root.path(), &[], true).unwrap();
        assert_eq!(
            names(&fake),
            [
                (Kind::EdgeCase, "many-files"),
                (Kind::EdgeCase, "odd-names"),
                (Kind::Realistic, "notes"),
            ]
        );
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            root.path().join("edge-cases/odd-names"),
            root.path().join("realistic/linked"),
        )
        .unwrap();
        let real = load(root.path(), &[], false).unwrap();
        assert_eq!(names(&real), [(Kind::Realistic, "notes")]);
        // Nor through a linked folder of realistic scenarios.
        #[cfg(unix)]
        {
            let linked = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(
                root.path().join("edge-cases"),
                linked.path().join("realistic"),
            )
            .unwrap();
            let error = load(linked.path(), &[], false).map(|_| ()).unwrap_err();
            assert!(
                error.to_string().starts_with("found no scenarios"),
                "{error}"
            );
        }
        let error = load(root.path(), &["names".to_string()], false)
            .map(|_| ())
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "found only edge cases whose names contain names, which run only with the fake \
                 model; real models run the scenarios in {}/realistic",
                root.path().display()
            )
        );
    }

    #[test]
    fn refuses_two_scenarios_of_one_name() {
        let root = scenarios(&[(Kind::EdgeCase, "notes"), (Kind::Realistic, "notes")]);

        let error = load(root.path(), &[], true).map(|_| ()).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "two scenarios in {} are called notes; rename one",
                root.path().display()
            )
        );
    }

    #[test]
    fn takes_turns_between_the_models_before_the_efforts() {
        let model = |name: &str, effort: &str| Model::Real {
            url: AQUEDUCT.to_string(),
            name: name.to_string(),
            effort: effort.parse().unwrap(),
        };
        let models = [
            model("large", "default"),
            model("large", "low"),
            model("small", "default"),
            model("small", "low"),
        ];

        let order: Vec<usize> = taking_turns(&models)
            .into_iter()
            .map(|(index, _)| index)
            .collect();

        assert_eq!(order, [0, 2, 1, 3]);
    }

    #[test]
    fn names_the_folders_after_any_model_and_effort() {
        let folders = |arguments: &[&str]| {
            let args = Args::try_parse_from(["fair-witness-scenarios"].iter().chain(arguments))
                .map_err(|e| e.kind())?;
            let models = models(&args).map_err(|e| e.to_string());
            Ok(models.map(|models| models.iter().map(Model::folder).collect::<Vec<_>>()))
        };

        assert_eq!(
            folders(&[
                "--model",
                "openai/gpt-5",
                "--effort",
                "default",
                "--effort",
                "ultra"
            ]),
            Ok(Ok(vec![
                "openai_gpt-5-default".to_string(),
                "openai_gpt-5-ultra".to_string()
            ]))
        );
        assert_eq!(
            folders(&[
                "--model", "large", "--effort", "../max", "--effort", ".._max"
            ]),
            Ok(Err(
                "two of the models would write their events to large-.._max; name each \
                    model and effort once"
                    .to_string()
            ))
        );
        assert_eq!(
            folders(&["--model", "large", "--effort", ""]),
            Err(clap::error::ErrorKind::ValueValidation)
        );
    }

    #[test]
    fn reads_the_api_url_as_fair_witness_does() {
        let url = |url: &str| {
            let args = Args::try_parse_from([
                "fair-witness-scenarios",
                "--model",
                "large",
                "--ai-url",
                url,
            ])
            .map_err(|e| e.kind())?;
            let Model::Real { url, .. } = models(&args).unwrap().remove(0) else {
                panic!("no real model");
            };
            Ok(url)
        };

        assert_eq!(
            [
                "models.example.org/v1",
                " http://127.0.0.1:8080/v1/",
                "ftp://models.example.org"
            ]
            .map(url),
            [
                Ok("https://models.example.org/v1".to_string()),
                Ok("http://127.0.0.1:8080/v1".to_string()),
                Err(clap::error::ErrorKind::ValueValidation)
            ]
        );
    }
}
