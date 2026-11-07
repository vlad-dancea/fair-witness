use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Result;
use jiff::{Timestamp, Unit};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

use crate::project::{self, Project};
use crate::review::{self, Refused, Step, Trace, Verdict};
use crate::watcher::{self, Change, Filter, FolderWatcher, WatchError};

/// How long a change set whose review failed waits before each further attempt. Its reviews have
/// already waited out what goes away within minutes, so these outlast an API that is down for
/// half an hour, without asking again and again one that stays down.
pub const RETRY_WAITS: [Duration; 5] = [
    Duration::from_secs(60),
    Duration::from_secs(2 * 60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
    Duration::from_secs(15 * 60),
];

/// Something that happened while watching, with the number of the change set it belongs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A change set, numbered from 1 in the order seen. It comes as soon as the set ends, also
    /// while an earlier set is reviewed, and its review waits for those before it.
    Changes {
        set: usize,
        changes: Vec<Change>,
    },
    Reviewing {
        set: usize,
    },
    Trace {
        set: usize,
        trace: Trace,
    },
    Verdict {
        set: usize,
        verdict: Verdict,
    },
    /// A review of the set failed, and the set stays undecided. With `retry_in_ms` it goes again
    /// after that long, or sooner once any set gets a verdict. Without it, it goes again only when
    /// another model is chosen, or else in the next run.
    ReviewFailed {
        set: usize,
        error: String,
        /// Which attempt at the set failed, from 1. A log from before there were more has none.
        #[serde(default = "first_attempt")]
        attempt: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_in_ms: Option<u64>,
    },
    /// The verdict on the newer set `by` covers all that was left of this failed set, so it goes
    /// to no further review.
    Settled {
        set: usize,
        by: usize,
    },
    /// Watching failed for some folders, or the system dropped events and the changes found by
    /// looking through the folder again come in the next set, after change set `set`, or
    /// before the first one when it is 0.
    WatchErrors {
        set: usize,
        errors: Vec<String>,
    },
    /// Watching ended for good, such as when the project folder was moved or deleted, and the
    /// session ends with it, dropping the reviews still to come. Only a `LogStopped` can follow,
    /// when this event could not be logged.
    WatchStopped {
        error: String,
    },
    /// The log could not be written, so it holds no events from here on, not even this one.
    LogStopped {
        error: String,
    },
}

fn first_attempt() -> usize {
    1
}

impl Event {
    /// The change set that nothing more can happen to after this event. A failed set is not
    /// decided, not even one that goes to no further attempt in this run.
    pub fn decided_set(&self) -> Option<usize> {
        match self {
            Event::Verdict { set, .. } | Event::Settled { set, .. } => Some(*set),
            _ => None,
        }
    }
}

/// An event with the time it happened, as the log holds it, one per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub time: Timestamp,
    #[serde(flatten)]
    pub event: Event,
}

/// Watches a project folder and, given a DMP, reviews each change set against it.
pub struct Session {
    project: Project,
    debounce: Duration,
    review: Option<ReviewSettings>,
    retry_waits: Vec<Duration>,
    another_model: Arc<Notify>,
    log: Option<PathBuf>,
}

struct ReviewSettings {
    dmp: String,
    steps: Vec<Arc<dyn Step>>,
}

impl Session {
    /// A session that reports changes without reviewing them.
    pub fn new(project: Project) -> Self {
        Self {
            project,
            debounce: watcher::DEBOUNCE,
            review: None,
            retry_waits: RETRY_WAITS.to_vec(),
            another_model: Arc::default(),
            log: None,
        }
    }

    /// Takes changes as one change set once the folder has been quiet for `debounce`.
    pub fn debounce(self, debounce: Duration) -> Self {
        Self { debounce, ..self }
    }

    /// Reviews each change set against `dmp`, running `steps` in order until one decides.
    pub fn reviewing(self, dmp: String, steps: Vec<Arc<dyn Step>>) -> Self {
        Self {
            review: Some(ReviewSettings { dmp, steps }),
            ..self
        }
    }

    /// Waits `waits` before the further attempts at a failed set instead of `RETRY_WAITS`.
    pub fn retry_waits(self, waits: Vec<Duration>) -> Self {
        Self {
            retry_waits: waits,
            ..self
        }
    }

    /// Sends every failed set again when `another_model` is notified, with its waits from the
    /// start, since what failed may have been the model.
    pub fn trying_again_on(self, another_model: Arc<Notify>) -> Self {
        Self {
            another_model,
            ..self
        }
    }

    /// Also adds every event to the file at `path`, as a line of JSON.
    pub fn logging_to(self, path: PathBuf) -> Self {
        Self {
            log: Some(path),
            ..self
        }
    }

    /// Starts watching. The session runs until the receiver is dropped or watching stops, and
    /// drops the review in progress and the failed sets then.
    pub fn start(self) -> Result<UnboundedReceiver<Record>> {
        let watcher = FolderWatcher::new(self.project.root(), self.debounce)?;
        // The rules the watcher read when it started, so that the model may read exactly what
        // the watcher reports.
        let filter = watcher.filter().clone();
        let log = self
            .log
            .as_deref()
            .map(project::append_private)
            .transpose()?;
        let (sender, received) = mpsc::unbounded_channel();
        let events = Arc::new(Events::new(sender, log));
        let (queue, queued) = mpsc::unbounded_channel();
        let reviews = tokio::spawn({
            let events = events.clone();
            async move {
                tokio::select! {
                    () = self.review_queued(queued, &filter, &events) => {}
                    () = events.sender.closed() => {}
                }
            }
        });
        // The watcher runs in a task of its own, so that a long review holds up neither the
        // watcher's timing of the next set nor its look at the files the next events name,
        // which may be gone by the time the review ends.
        tokio::spawn(async move {
            let stopped = tokio::select! {
                stopped = watch(watcher, &events, &queue) => stopped,
                () = events.sender.closed() => None,
                // The reviews are gone only after a panic, and then the session ends at once
                // rather than look as if it still reviewed.
                () = queue.closed() => None,
            };
            if let Some(error) = stopped {
                // No review can read the folder any more, and why watching stopped comes last.
                reviews.abort();
                let _ = reviews.await;
                events.send(Event::WatchStopped { error });
            }
        });
        Ok(received)
    }

    // Reviews run one after another, so a review that newer changes make moot still finishes, and
    // the sets that came meanwhile wait in `queued`. A failed set that is due goes first, the
    // oldest of them, and until then it holds up nothing. A review stays limited by its timeout,
    // so the sets in `queued` always get their turn.
    async fn review_queued(
        &self,
        mut queued: UnboundedReceiver<(usize, Vec<Change>)>,
        filter: &Filter,
        events: &Events,
    ) {
        let Some(settings) = &self.review else {
            while queued.recv().await.is_some() {}
            return;
        };
        let mut failed = FailedSets::new(self.retry_waits.clone());
        let mut more_to_come = true;
        loop {
            // A model chosen during the last review counts before the next one starts, or a set
            // that the new model refuses would go again for a choice it was already refused under.
            tokio::select! {
                biased;
                () = self.another_model.notified() => failed.start_over(),
                () = std::future::ready(()) => {}
            }
            let (set, undecided) = match failed.take_due(Instant::now()) {
                Some(due) => due,
                None => {
                    let next_due = failed.next_due();
                    if !more_to_come && next_due.is_none() {
                        return;
                    }
                    tokio::select! {
                        biased;
                        () = self.another_model.notified() => {
                            failed.start_over();
                            continue;
                        }
                        () = sleep_until(next_due) => continue,
                        next = queued.recv(), if more_to_come => match next {
                            Some((set, changes)) => (set, Undecided::new(changes)),
                            None => {
                                more_to_come = false;
                                continue;
                            }
                        },
                    }
                }
            };
            match self
                .review_set(settings, set, &undecided.changes, filter, events)
                .await
            {
                Ok(verdict) => {
                    events.send(Event::Verdict { set, verdict });
                    failed.settle(set, &undecided.changes, events);
                }
                Err(e) => events.send(failed.add(set, undecided, &e)),
            }
        }
    }

    async fn review_set(
        &self,
        settings: &ReviewSettings,
        set: usize,
        changes: &[Change],
        filter: &Filter,
        events: &Events,
    ) -> Result<Verdict> {
        events.send(Event::Reviewing { set });
        let trace = |trace| events.send(Event::Trace { set, trace });
        let root = self.project.root();
        review::run_steps(
            &settings.steps,
            root,
            filter,
            &settings.dmp,
            changes,
            &trace,
        )
        .await
    }
}

/// Resolves at `due`, and without one never.
async fn sleep_until(due: Option<Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(due).await,
        None => std::future::pending().await,
    }
}

/// A change set on its way to a verdict.
struct Undecided {
    /// What is left of it, without what a newer set's verdict covered.
    changes: Vec<Change>,
    /// How many of its reviews failed.
    failures: usize,
    /// How many of the waits it has had.
    waited: usize,
    /// When it goes again by itself, if it does.
    due: Option<Instant>,
}

impl Undecided {
    fn new(changes: Vec<Change>) -> Self {
        Self {
            changes,
            failures: 0,
            waited: 0,
            due: None,
        }
    }
}

/// The change sets whose reviews failed, oldest first, which never merge, so that each number
/// keeps one verdict.
struct FailedSets {
    waits: Vec<Duration>,
    sets: BTreeMap<usize, Undecided>,
}

impl FailedSets {
    fn new(waits: Vec<Duration>) -> Self {
        Self {
            waits,
            sets: BTreeMap::new(),
        }
    }

    /// Takes the oldest set that is due at `now`.
    fn take_due(&mut self, now: Instant) -> Option<(usize, Undecided)> {
        let set = self
            .sets
            .iter()
            .find(|(_, undecided)| undecided.due.is_some_and(|due| due <= now))
            .map(|(set, _)| *set)?;
        self.sets.remove_entry(&set)
    }

    fn next_due(&self) -> Option<Instant> {
        self.sets.values().filter_map(|set| set.due).min()
    }

    /// Keeps `set` for its next attempt after a review that failed with `error`, and gives the
    /// event that says so. A refusal gets no timed attempts, since it would only come back.
    fn add(&mut self, set: usize, mut undecided: Undecided, error: &anyhow::Error) -> Event {
        undecided.failures += 1;
        let wait = match error.downcast_ref::<Refused>() {
            Some(_) => None,
            None => self.waits.get(undecided.waited).copied(),
        };
        undecided.waited += usize::from(wait.is_some());
        undecided.due = wait.map(|wait| Instant::now() + wait);
        let attempt = undecided.failures;
        self.sets.insert(set, undecided);
        Event::ReviewFailed {
            set,
            error: format!("{error:#}"),
            attempt,
            retry_in_ms: wait.map(|wait| u64::try_from(wait.as_millis()).unwrap_or(u64::MAX)),
        }
    }

    /// Takes in the verdict on the set `by`, for `changes`. A review reads the folder as it is
    /// then, so the verdict also covers those paths in older sets, and it shows that the API
    /// works again, so the waits end. A refusal stays, since a verdict does not lift it.
    fn settle(&mut self, by: usize, changes: &[Change], events: &Events) {
        let covered: HashSet<&Path> = changes.iter().flat_map(Change::paths).collect();
        self.sets.retain(|&set, undecided| {
            if set > by {
                return true;
            }
            undecided
                .changes
                .retain(|change| !change.paths().iter().all(|path| covered.contains(path)));
            if undecided.changes.is_empty() {
                events.send(Event::Settled { set, by });
            }
            !undecided.changes.is_empty()
        });
        let now = Instant::now();
        for undecided in self.sets.values_mut() {
            if undecided.due.is_some() {
                undecided.due = Some(now);
            }
        }
    }

    /// Makes every set due at once, with all its waits still to come.
    fn start_over(&mut self) {
        let now = Instant::now();
        for undecided in self.sets.values_mut() {
            undecided.waited = 0;
            undecided.due = Some(now);
        }
    }
}

/// Numbers each change set the watcher ends, reports it and queues it for review, and gives
/// the error that stopped watching, if one did.
async fn watch(
    mut watcher: FolderWatcher,
    events: &Events,
    queue: &UnboundedSender<(usize, Vec<Change>)>,
) -> Option<String> {
    let mut set = 0;
    while let Some(batch) = watcher.next().await {
        let changes = match batch {
            Ok(changes) => changes,
            Err(WatchError::Failed(errors)) => {
                events.send(Event::WatchErrors { set, errors });
                continue;
            }
            Err(WatchError::Gone) => {
                return Some(
                    "the project folder was moved or deleted, so it is no longer watched; start \
                     `fair-witness run` again in its new place"
                        .to_string(),
                );
            }
            Err(WatchError::Stopped(error)) => return Some(error),
        };
        set += 1;
        events.send(Event::Changes {
            set,
            changes: changes.clone(),
        });
        if queue.send((set, changes)).is_err() {
            return None;
        }
    }
    None
}

/// Stamps each event with the time, logs it and passes it on, leaving out what a review
/// repeats from the one before.
struct Events {
    sender: UnboundedSender<Record>,
    log: Mutex<Option<File>>,
    last_setup: Mutex<Option<Trace>>,
    last_model: Mutex<Option<Trace>>,
}

impl Events {
    fn new(sender: UnboundedSender<Record>, log: Option<File>) -> Self {
        Self {
            sender,
            log: Mutex::new(log),
            last_setup: Mutex::new(None),
            last_model: Mutex::new(None),
        }
    }

    fn send(&self, event: Event) {
        if self.repeats(&event) {
            return;
        }
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        // Taken under the lock, so that the events of the watcher and of a review, which send
        // from tasks of their own, are logged and passed on in the order of their times.
        let record = Record { time: now(), event };
        if let Some(file) = log.as_mut() {
            let end = file.metadata().map(|metadata| metadata.len());
            let written = serde_json::to_vec(&record)
                .map_err(std::io::Error::from)
                .and_then(|mut line| {
                    line.push(b'\n');
                    file.write_all(&line)
                });
            // A gap in the middle of the log would read as events that never happened.
            if let Err(e) = written {
                // Part of the line may be written, and a log ending in half a line of JSON
                // could not be read back.
                if let Ok(end) = end {
                    let _ = file.set_len(end);
                }
                tracing::error!("stopped writing the event log: {e}");
                *log = None;
                let _ = self.sender.send(record);
                let stopped = Event::LogStopped {
                    error: e.to_string(),
                };
                let _ = self.sender.send(Record {
                    time: now(),
                    event: stopped,
                });
                return;
            }
        }
        let _ = self.sender.send(record);
    }

    /// Whether `event` is the setup or model of a review that the last one already reported.
    fn repeats(&self, event: &Event) -> bool {
        let (last, trace) = match event {
            Event::Trace {
                trace: trace @ Trace::Setup { .. },
                ..
            } => (&self.last_setup, trace),
            Event::Trace {
                trace: trace @ Trace::Model(_),
                ..
            } => (&self.last_model, trace),
            _ => return false,
        };
        let mut last = last.lock().unwrap_or_else(PoisonError::into_inner);
        if last.as_ref() == Some(trace) {
            return true;
        }
        *last = Some(trace.clone());
        false
    }
}

/// The time to the millisecond, which is as fine as the log needs.
fn now() -> Timestamp {
    let now = Timestamp::now();
    now.round(Unit::Millisecond).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;

    use serde_json::json;

    use super::*;
    use crate::model::{Effort, ModelChoice};
    use crate::project::AiConfig;
    use crate::review::{Input, ModelStep, StepFuture, StepOutcome};
    use crate::testing::{Reply, fake_model, held, no_answer, too_many_requests, tool_call};

    /// Long enough for the watcher to wait out its debounce and the review to finish.
    const PATIENCE: Duration = Duration::from_secs(10);

    async fn until_decided(events: &mut UnboundedReceiver<Record>) -> Vec<Record> {
        let mut seen = Vec::new();
        loop {
            let event = tokio::time::timeout(PATIENCE, events.recv())
                .await
                .expect("no verdict in time")
                .expect("the session stopped");
            let decided = matches!(
                event.event,
                Event::Verdict { .. } | Event::ReviewFailed { .. }
            );
            seen.push(event);
            if decided {
                return seen;
            }
        }
    }

    /// Reads events until the one that `describe` gives as `wanted`, and gives back what it read.
    async fn until(events: &mut UnboundedReceiver<Record>, wanted: &str) -> Vec<Record> {
        let mut seen = Vec::new();
        loop {
            let event = tokio::time::timeout(PATIENCE, events.recv())
                .await
                .unwrap_or_else(|_| panic!("no {wanted:?} in time"))
                .expect("the session stopped");
            let found = describe(&event.event) == wanted;
            seen.push(event);
            if found {
                return seen;
            }
        }
    }

    fn reviewer(endpoint: String) -> Arc<dyn Step> {
        let config = AiConfig {
            url: endpoint,
            choice: ModelChoice {
                model: "test".to_string(),
                effort: Effort::default(),
            },
        };
        Arc::new(ModelStep::new(&config, None).unwrap())
    }

    fn describe(event: &Event) -> String {
        match event {
            Event::Changes { set, changes } => format!("#{set} changes {changes:?}"),
            Event::Reviewing { set } => format!("#{set} reviewing"),
            Event::Trace { set, trace } => match trace {
                Trace::Setup { dmp, .. } => format!("#{set} setup with DMP {dmp}"),
                Trace::Model(choice) => format!("#{set} model {}", choice.model),
                Trace::Prompt(text) => {
                    format!("#{set} prompt {}", text.lines().nth(1).unwrap_or(""))
                }
                Trace::Sent(text) => format!("#{set} sent {text}"),
                Trace::Request { turn } => format!("#{set} request {turn}"),
                Trace::RateLimited { turn, wait_ms, .. } => {
                    format!("#{set} request {turn} waits {wait_ms} ms")
                }
                Trace::Retrying {
                    turn, wait_ms, why, ..
                } => format!("#{set} request {turn} waits {wait_ms} ms: {why}"),
                Trace::EffortNotAllowed { turn, .. } => {
                    format!("#{set} request {turn} goes again with allowed_openai_params")
                }
                Trace::Reply { turn, calls, .. } => format!("#{set} reply {turn}: {}", calls[0].0),
                Trace::ToolResult { name, output, .. } => format!("#{set} {name}: {output}"),
                Trace::Failed(e) => format!("#{set} failed: {e}"),
            },
            Event::Verdict { set, verdict } => format!(
                "#{set} verdict: contradicts {} ({:?}): {}",
                verdict.contradicts, verdict.dmp_field, verdict.explanation
            ),
            Event::ReviewFailed {
                set,
                error,
                attempt,
                retry_in_ms,
            } => match retry_in_ms {
                Some(wait) => {
                    format!("#{set} attempt {attempt} failed, again in {wait} ms: {error}")
                }
                None => format!("#{set} attempt {attempt} failed, no more by itself: {error}"),
            },
            Event::Settled { set, by } => format!("#{set} settled by #{by}"),
            Event::WatchErrors { set, errors } => format!("#{set} watch errors: {errors:?}"),
            Event::WatchStopped { error } => format!("watching stopped: {error}"),
            Event::LogStopped { error } => format!("log stopped: {error}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn says_once_when_the_log_cannot_be_written() {
        // Every write to /dev/full fails as if the disk were full.
        let full = File::options().write(true).open("/dev/full").unwrap();
        let (sender, mut received) = mpsc::unbounded_channel();
        let events = Events::new(sender, Some(full));

        events.send(Event::Reviewing { set: 1 });
        events.send(Event::Reviewing { set: 2 });
        drop(events);

        let mut seen = Vec::new();
        while let Some(record) = received.recv().await {
            seen.push(describe(&record.event));
        }
        assert_eq!(
            seen,
            [
                "#1 reviewing",
                "log stopped: No space left on device (os error 28)",
                "#2 reviewing",
            ]
        );
    }

    /// A step with a bug that makes it panic.
    struct Panics;

    impl Step for Panics {
        fn decide<'a>(&'a self, _: &'a Input<'a>, _: &'a (dyn Fn(Trace) + Sync)) -> StepFuture<'a> {
            Box::pin(async { panic!("a step of the review panicked, as this test wants") })
        }
    }

    #[tokio::test]
    async fn ends_when_a_review_panics() {
        let root = tempfile::tempdir().unwrap();
        let mut events = Session::new(Project::new(root.path().to_path_buf()))
            .debounce(Duration::from_millis(50))
            .reviewing("{}".to_string(), vec![Arc::new(Panics)])
            .start()
            .unwrap();

        // Nothing changes after the first set, so only the session's end can end the events.
        fs::write(root.path().join("results.csv"), "a").unwrap();
        let mut seen = Vec::new();
        while let Some(record) = tokio::time::timeout(PATIENCE, events.recv())
            .await
            .expect("the session went on after its review panicked")
        {
            seen.push(describe(&record.event));
        }

        assert_eq!(
            seen,
            [r#"#1 changes [Created("results.csv")]"#, "#1 reviewing"]
        );
    }

    /// A step that never decides, as a slow model.
    struct Stalls;

    impl Step for Stalls {
        fn decide<'a>(&'a self, _: &'a Input<'a>, _: &'a (dyn Fn(Trace) + Sync)) -> StepFuture<'a> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn ends_with_why_once_the_project_folder_is_moved() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("project");
        fs::create_dir(&root).unwrap();
        let mut events = Session::new(Project::new(root.clone()))
            .debounce(Duration::from_millis(50))
            .reviewing("{}".to_string(), vec![Arc::new(Stalls)])
            .start()
            .unwrap();

        fs::write(root.join("results.csv"), "a").unwrap();
        let mut seen: Vec<String> = until(&mut events, "#1 reviewing")
            .await
            .iter()
            .map(|record| describe(&record.event))
            .collect();
        fs::rename(&root, parent.path().join("moved")).unwrap();
        // The review in progress never decides, so only the session's end can end the events.
        while let Some(record) = tokio::time::timeout(PATIENCE, events.recv())
            .await
            .expect("the session went on after its folder moved")
        {
            seen.push(describe(&record.event));
        }

        assert_eq!(
            seen,
            [
                r#"#1 changes [Created("results.csv")]"#,
                "#1 reviewing",
                "watching stopped: the project folder was moved or deleted, so it is no longer \
                 watched; start `fair-witness run` again in its new place",
            ]
        );
    }

    #[tokio::test]
    async fn collects_the_changes_made_during_a_review_in_sets_of_their_own() {
        let root = tempfile::tempdir().unwrap();
        let in_line = |explanation: &str| -> Reply {
            let verdict = json!({ "contradicts": false, "explanation": explanation });
            tool_call("report_verdict", verdict).into()
        };
        let (release, released) = std::sync::mpsc::channel();
        let (endpoint, server) = fake_model(vec![
            held(in_line("Results, as planned."), released),
            in_line("New results, as planned."),
            in_line("Nothing is left to check."),
        ]);
        let mut events = Session::new(Project::new(root.path().to_path_buf()))
            .debounce(Duration::from_millis(50))
            .reviewing("{}".to_string(), vec![reviewer(endpoint)])
            .start()
            .unwrap();
        let results = root.path().join("results.csv");

        // The researcher edits the file and then deletes it while the model takes its time with
        // the first set.
        fs::write(&results, "a").unwrap();
        let mut seen = until(&mut events, "#1 request 1").await;
        fs::write(&results, "b").unwrap();
        seen.extend(until(&mut events, r#"#2 changes [Changed("results.csv")]"#).await);
        fs::remove_file(&results).unwrap();
        seen.extend(until(&mut events, r#"#3 changes [Deleted("results.csv")]"#).await);
        release.send(()).unwrap();
        seen.extend(
            until(
                &mut events,
                "#3 verdict: contradicts false (None): Nothing is left to check.",
            )
            .await,
        );

        assert_eq!(
            seen.iter()
                .map(|record| describe(&record.event))
                .collect::<Vec<_>>(),
            [
                r#"#1 changes [Created("results.csv")]"#,
                "#1 reviewing",
                "#1 setup with DMP {}",
                "#1 model test",
                "#1 prompt created: results.csv",
                "#1 request 1",
                r#"#2 changes [Changed("results.csv")]"#,
                r#"#3 changes [Deleted("results.csv")]"#,
                "#1 reply 1: report_verdict",
                "#1 verdict: contradicts false (None): Results, as planned.",
                "#2 reviewing",
                "#2 prompt changed: results.csv (gone since, so it can no longer be read)",
                "#2 request 1",
                "#2 reply 1: report_verdict",
                "#2 verdict: contradicts false (None): New results, as planned.",
                "#3 reviewing",
                "#3 prompt deleted: results.csv",
                "#3 request 1",
                "#3 reply 1: report_verdict",
                "#3 verdict: contradicts false (None): Nothing is left to check.",
            ]
        );
        assert_eq!(server.join().unwrap().len(), 3);
        assert!(seen.is_sorted_by_key(|record| record.time));
    }

    #[tokio::test]
    async fn reviews_each_change_set_and_logs_its_numbered_events() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, server) = fake_model(vec![
            tool_call(
                "report_verdict",
                json!({ "contradicts": false, "explanation": "Notes are not data." }),
            ),
            tool_call("read_file", json!({ "path": "patients.csv" })),
            tool_call(
                "report_verdict",
                json!({
                    "contradicts": true,
                    "dmp_field": "dataset[0].personal_data",
                    "explanation": "Patient names, but the DMP says no personal data."
                }),
            ),
        ]);
        let project = Project::new(root.path().to_path_buf());
        let log = project.run_log().unwrap().events;
        let mut events = Session::new(project)
            .debounce(Duration::from_millis(50))
            .reviewing("{}".to_string(), vec![reviewer(endpoint)])
            .logging_to(log.clone())
            .start()
            .unwrap();

        fs::write(root.path().join("notes.txt"), "Ask about consent").unwrap();
        let mut seen = until_decided(&mut events).await;
        fs::write(root.path().join("patients.csv"), "name\nAnna Muster\n").unwrap();
        seen.extend(until_decided(&mut events).await);

        assert_eq!(
            seen.iter()
                .map(|record| describe(&record.event))
                .collect::<Vec<_>>(),
            [
                r#"#1 changes [Created("notes.txt")]"#,
                "#1 reviewing",
                "#1 setup with DMP {}",
                "#1 model test",
                "#1 prompt created: notes.txt",
                "#1 request 1",
                "#1 reply 1: report_verdict",
                "#1 verdict: contradicts false (None): Notes are not data.",
                r#"#2 changes [Created("patients.csv")]"#,
                "#2 reviewing",
                "#2 prompt created: patients.csv",
                "#2 request 1",
                "#2 reply 1: read_file",
                "#2 read_file: name\nAnna Muster\n",
                "#2 request 2",
                "#2 reply 2: report_verdict",
                "#2 verdict: contradicts true (Some(\"dataset[0].personal_data\")): \
                 Patient names, but the DMP says no personal data.",
            ]
        );
        assert_eq!(server.join().unwrap().len(), 3);
        assert!(seen.is_sorted_by_key(|record| record.time));
        let logged: Vec<Record> = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(logged, seen);
        let mut first = serde_json::to_value(&seen[0]).unwrap();
        let time = first.as_object_mut().unwrap().remove("time").unwrap();
        assert_eq!(time, json!(seen[0].time.to_string()));
        assert_eq!(
            first,
            json!({ "event": "changes", "set": 1, "changes": [{ "created": "notes.txt" }] })
        );
        let mut reply = serde_json::to_value(&seen[6].event).unwrap();
        reply["trace"]["reply"]
            .as_object_mut()
            .unwrap()
            .remove("elapsed_ms")
            .unwrap();
        assert_eq!(
            reply,
            json!({ "event": "trace", "set": 1, "trace": { "reply": {
                "turn": 1,
                "calls": [["report_verdict", {
                    "contradicts": false, "explanation": "Notes are not data."
                }]],
                "stop_reason": "tool_calls",
                "tokens_in": 1,
                "tokens_out": 1
            } } })
        );
    }

    #[tokio::test]
    async fn fails_a_change_set_the_api_does_not_answer_and_reviews_the_next() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, server) = fake_model(vec![
            no_answer(),
            tool_call(
                "report_verdict",
                json!({ "contradicts": false, "explanation": "Survey answers, as planned." }),
            )
            .into(),
        ]);
        let reviewer = ModelStep::with_timeouts(
            &AiConfig {
                url: endpoint,
                choice: ModelChoice {
                    model: "test".to_string(),
                    effort: Effort::default(),
                },
            },
            None,
            Duration::from_secs(1),
            Duration::from_secs(2),
        )
        .unwrap();
        let mut events = Session::new(Project::new(root.path().to_path_buf()))
            .debounce(Duration::from_millis(50))
            .reviewing("{}".to_string(), vec![Arc::new(reviewer)])
            .start()
            .unwrap();

        fs::write(root.path().join("notes.txt"), "Ask about consent").unwrap();
        let mut seen = until_decided(&mut events).await;
        fs::write(root.path().join("survey.csv"), "age\n42\n").unwrap();
        seen.extend(until_decided(&mut events).await);

        assert_eq!(
            seen.iter()
                .map(|record| describe(&record.event))
                .collect::<Vec<_>>(),
            [
                r#"#1 changes [Created("notes.txt")]"#,
                "#1 reviewing",
                "#1 setup with DMP {}",
                "#1 model test",
                "#1 prompt created: notes.txt",
                "#1 request 1",
                "#1 request 1 waits 2000 ms: the API did not answer within 1 s",
                "#1 failed: the model test gave no verdict within 2 s",
                "#1 attempt 1 failed, again in 60000 ms: the model test gave no verdict within 2 s",
                r#"#2 changes [Created("survey.csv")]"#,
                "#2 reviewing",
                "#2 prompt created: survey.csv",
                "#2 request 1",
                "#2 reply 1: report_verdict",
                "#2 verdict: contradicts false (None): Survey answers, as planned.",
            ]
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    /// What the fake API answers when the day's quota is used up, which fails a review at once.
    fn out_of_quota() -> Reply {
        too_many_requests(Some("86400"), "Daily quota exceeded.")
    }

    const OUT_OF_QUOTA: &str = "the model test answered 429 Too Many Requests and asks to wait \
                                86400 s, longer than fair-witness waits (60 s): the API answered \
                                429 Too Many Requests: Daily quota exceeded.";

    fn in_line(explanation: &str) -> Reply {
        let verdict = json!({ "contradicts": false, "explanation": explanation });
        tool_call("report_verdict", verdict).into()
    }

    fn descriptions(records: &[Record]) -> Vec<String> {
        records
            .iter()
            .map(|record| describe(&record.event))
            .collect()
    }

    #[tokio::test]
    async fn reviews_a_failed_set_again_under_its_number_once_a_later_set_gets_its_verdict() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, server) = fake_model(vec![
            out_of_quota(),
            in_line("Survey answers, as planned."),
            in_line("Notes are not data."),
        ]);
        let mut events = Session::new(Project::new(root.path().to_path_buf()))
            .debounce(Duration::from_millis(50))
            .reviewing("{}".to_string(), vec![reviewer(endpoint)])
            .start()
            .unwrap();

        fs::write(root.path().join("notes.txt"), "Ask about consent").unwrap();
        let mut seen = until_decided(&mut events).await;
        fs::write(root.path().join("survey.csv"), "age\n42\n").unwrap();
        seen.extend(
            until(
                &mut events,
                "#1 verdict: contradicts false (None): Notes are not data.",
            )
            .await,
        );

        assert_eq!(
            descriptions(&seen),
            [
                r#"#1 changes [Created("notes.txt")]"#.to_string(),
                "#1 reviewing".to_string(),
                "#1 setup with DMP {}".to_string(),
                "#1 model test".to_string(),
                "#1 prompt created: notes.txt".to_string(),
                "#1 request 1".to_string(),
                "#1 failed: the API answered 429 Too Many Requests: Daily quota exceeded."
                    .to_string(),
                format!("#1 attempt 1 failed, again in 60000 ms: {OUT_OF_QUOTA}"),
                r#"#2 changes [Created("survey.csv")]"#.to_string(),
                "#2 reviewing".to_string(),
                "#2 prompt created: survey.csv".to_string(),
                "#2 request 1".to_string(),
                "#2 reply 1: report_verdict".to_string(),
                "#2 verdict: contradicts false (None): Survey answers, as planned.".to_string(),
                "#1 reviewing".to_string(),
                "#1 prompt created: notes.txt".to_string(),
                "#1 request 1".to_string(),
                "#1 reply 1: report_verdict".to_string(),
                "#1 verdict: contradicts false (None): Notes are not data.".to_string(),
            ]
        );
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn settles_a_failed_set_when_a_newer_verdict_covers_all_its_paths() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, server) = fake_model(vec![
            out_of_quota(),
            in_line("New results, as planned."),
            in_line("Notes are not data."),
        ]);
        let mut events = Session::new(Project::new(root.path().to_path_buf()))
            .debounce(Duration::from_millis(50))
            .reviewing("{}".to_string(), vec![reviewer(endpoint)])
            .start()
            .unwrap();
        let results = root.path().join("results.csv");

        fs::write(&results, "a").unwrap();
        until_decided(&mut events).await;
        fs::write(&results, "b").unwrap();
        until(
            &mut events,
            "#2 verdict: contradicts false (None): New results, as planned.",
        )
        .await;
        let settled = events.recv().await.unwrap();
        // The next set goes straight to the model, with nothing for the settled set before it.
        fs::write(root.path().join("notes.txt"), "Ask about consent").unwrap();
        let next = until(
            &mut events,
            "#3 verdict: contradicts false (None): Notes are not data.",
        )
        .await;

        assert_eq!(settled.event, Event::Settled { set: 1, by: 2 });
        assert_eq!(settled.event.decided_set(), Some(1));
        assert_eq!(
            descriptions(&next),
            [
                r#"#3 changes [Created("notes.txt")]"#,
                "#3 reviewing",
                "#3 prompt created: notes.txt",
                "#3 request 1",
                "#3 reply 1: report_verdict",
                "#3 verdict: contradicts false (None): Notes are not data.",
            ]
        );
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn reviews_only_what_a_newer_verdict_left_of_a_failed_set() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, server) = fake_model(vec![
            out_of_quota(),
            in_line("New results, as planned."),
            in_line("Notes are not data."),
        ]);
        let mut events = Session::new(Project::new(root.path().to_path_buf()))
            .debounce(Duration::from_millis(50))
            .reviewing("{}".to_string(), vec![reviewer(endpoint)])
            .start()
            .unwrap();
        let results = root.path().join("results.csv");

        fs::write(&results, "a").unwrap();
        fs::write(root.path().join("notes.txt"), "Ask about consent").unwrap();
        until_decided(&mut events).await;
        fs::write(&results, "b").unwrap();
        let seen = until(
            &mut events,
            "#1 verdict: contradicts false (None): Notes are not data.",
        )
        .await;

        assert!(
            descriptions(&seen).contains(&"#1 prompt created: notes.txt".to_string()),
            "{seen:?}"
        );
        let requests = server.join().unwrap();
        let prompt = requests[2].1["messages"][1]["content"].as_str().unwrap();
        assert!(prompt.ends_with("created: notes.txt"), "{prompt}");
        assert!(!prompt.contains("results.csv"), "{prompt}");
    }

    #[test]
    fn covers_a_rename_only_with_both_its_names() {
        let changed = |path: &str| Change::Changed(path.into());
        let renamed = Change::Renamed {
            from: "a.csv".into(),
            to: "b.csv".into(),
        };
        let (sender, mut received) = mpsc::unbounded_channel();
        let events = Events::new(sender, None);
        let mut failed = FailedSets::new(Vec::new());
        failed.add(
            2,
            Undecided::new(vec![renamed.clone()]),
            &anyhow::anyhow!("down"),
        );

        failed.settle(3, &[changed("b.csv")], &events);
        assert_eq!(failed.sets[&2].changes, [renamed]);
        // A verdict on an older set covers nothing of a newer one.
        failed.settle(1, &[changed("a.csv"), changed("b.csv")], &events);
        assert!(received.try_recv().is_err());

        failed.settle(3, &[changed("a.csv"), changed("b.csv")], &events);
        assert!(failed.sets.is_empty());
        assert_eq!(
            received.try_recv().unwrap().event,
            Event::Settled { set: 2, by: 3 }
        );
    }

    /// A step that gives `answers` in turn and then fails, and notes when it was asked about what.
    struct Scripted {
        answers: Mutex<VecDeque<Result<Verdict>>>,
        asked: Mutex<Vec<(Instant, Vec<Change>)>>,
    }

    impl Scripted {
        fn new(answers: impl IntoIterator<Item = Result<Verdict>>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.into_iter().collect()),
                asked: Mutex::default(),
            })
        }

        /// The paths it was asked about, in turn.
        fn paths(&self) -> Vec<String> {
            let asked = self.asked.lock().unwrap();
            asked
                .iter()
                .map(|(_, changes)| changes[0].path().display().to_string())
                .collect()
        }

        /// How long after the first time it was asked each later time was.
        fn since_first(&self) -> Vec<Duration> {
            let asked = self.asked.lock().unwrap();
            asked
                .iter()
                .map(|(at, _)| at.duration_since(asked[0].0))
                .collect()
        }
    }

    impl Step for Scripted {
        fn decide<'a>(
            &'a self,
            input: &'a Input<'a>,
            _: &'a (dyn Fn(Trace) + Sync),
        ) -> StepFuture<'a> {
            self.asked
                .lock()
                .unwrap()
                .push((Instant::now(), input.changes.to_vec()));
            let answer = self.answers.lock().unwrap().pop_front();
            Box::pin(async {
                let verdict = answer.unwrap_or_else(|| Err(anyhow::anyhow!("the API is down")))?;
                Ok(StepOutcome::Decided(verdict))
            })
        }
    }

    fn fine() -> Result<Verdict> {
        Ok(Verdict {
            contradicts: false,
            dmp_field: None,
            explanation: "Fine.".to_string(),
        })
    }

    fn down() -> Result<Verdict> {
        Err(anyhow::anyhow!("the API is down"))
    }

    fn refused() -> Result<Verdict> {
        Err(anyhow::Error::new(Refused("the key expired".to_string())).context("the model failed"))
    }

    /// Change set `set`, which creates `path`.
    fn created(set: usize, path: &str) -> (usize, Vec<Change>) {
        (set, vec![Change::Created(path.into())])
    }

    /// What the reviews of a session report and what feeds them, without a watcher, so that a test
    /// can run in paused time, where the waits take no time.
    struct Reviews {
        session: Session,
        filter: Filter,
        events: Events,
        reported: UnboundedReceiver<Record>,
        _root: tempfile::TempDir,
    }

    impl Reviews {
        fn new(step: Arc<Scripted>) -> Self {
            let root = tempfile::tempdir().unwrap();
            let (sender, reported) = mpsc::unbounded_channel();
            Self {
                session: Session::new(Project::new(root.path().to_path_buf()))
                    .reviewing("{}".to_string(), vec![step]),
                filter: Filter::new(root.path()).unwrap(),
                events: Events::new(sender, None),
                reported,
                _root: root,
            }
        }

        /// Reviews what `queued` holds, with the channel still open while `meanwhile` runs, so that
        /// only the waits can decide what goes next.
        async fn run(
            &self,
            queued: UnboundedReceiver<(usize, Vec<Change>)>,
            meanwhile: impl Future<Output = ()>,
        ) {
            tokio::select! {
                () = self.session.review_queued(queued, &self.filter, &self.events) => {
                    panic!("the reviews ended while more could come");
                }
                () = meanwhile => {}
            }
        }

        /// The attempts that failed so far, with the wait each announced.
        fn failures(&mut self) -> Vec<(usize, usize, Option<u64>)> {
            let mut failures = Vec::new();
            while let Ok(record) = self.reported.try_recv() {
                if let Event::ReviewFailed {
                    set,
                    attempt,
                    retry_in_ms,
                    ..
                } = record.event
                {
                    failures.push((set, attempt, retry_in_ms));
                }
            }
            failures
        }
    }

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    #[tokio::test(start_paused = true)]
    async fn stops_after_five_more_attempts_at_the_set_waits() {
        let step = Scripted::new([]);
        let mut reviews = Reviews::new(step.clone());
        let (queue, queued) = mpsc::unbounded_channel();
        queue.send(created(1, "a.csv")).unwrap();
        // Also when no more sets can come, as after watching ended without an error.
        drop(queue);

        reviews
            .session
            .review_queued(queued, &reviews.filter, &reviews.events)
            .await;

        let minutes = |minutes: u64| Duration::from_secs(minutes * 60);
        assert_eq!(
            step.since_first(),
            [
                minutes(0),
                minutes(1),
                minutes(3),
                minutes(8),
                minutes(23),
                minutes(38)
            ]
        );
        assert_eq!(
            reviews.failures(),
            [
                (1, 1, Some(60_000)),
                (1, 2, Some(120_000)),
                (1, 3, Some(300_000)),
                (1, 4, Some(900_000)),
                (1, 5, Some(900_000)),
                (1, 6, None),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn makes_the_waiting_sets_due_at_once_oldest_first_with_any_verdict() {
        let step = Scripted::new([down(), down(), fine(), fine(), fine()]);
        let mut reviews = Reviews::new(step.clone());
        let (queue, queued) = mpsc::unbounded_channel();
        for (set, path) in [(1, "a.csv"), (2, "b.csv"), (3, "c.csv")] {
            queue.send(created(set, path)).unwrap();
        }

        reviews.run(queued, tokio::time::sleep(DAY)).await;

        assert_eq!(step.paths(), ["a.csv", "b.csv", "c.csv", "a.csv", "b.csv"]);
        assert_eq!(step.since_first(), [Duration::ZERO; 5]);
        assert_eq!(
            reviews.failures(),
            [(1, 1, Some(60_000)), (2, 1, Some(60_000))]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn holds_up_no_newer_set_while_a_failed_one_waits() {
        let step = Scripted::new([down(), fine(), fine()]);
        let reviews = Reviews::new(step.clone());
        let (queue, queued) = mpsc::unbounded_channel();
        queue.send(created(1, "a.csv")).unwrap();

        reviews
            .run(queued, async {
                // Well before the first wait ends.
                tokio::time::sleep(Duration::from_secs(1)).await;
                queue.send(created(2, "b.csv")).unwrap();
                tokio::time::sleep(DAY).await;
            })
            .await;

        assert_eq!(step.paths(), ["a.csv", "b.csv", "a.csv"]);
        assert_eq!(
            step.since_first(),
            [
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(1)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sends_a_refused_set_again_only_when_a_model_is_chosen() {
        let step = Scripted::new([refused(), fine(), fine()]);
        let another_model = Arc::new(Notify::new());
        let mut reviews = Reviews::new(step.clone());
        reviews.session = reviews.session.trying_again_on(another_model.clone());
        let (queue, queued) = mpsc::unbounded_channel();
        queue.send(created(1, "a.csv")).unwrap();

        let paths = Mutex::new(Vec::new());
        reviews
            .run(queued, async {
                tokio::time::sleep(DAY).await;
                paths.lock().unwrap().push(step.paths());
                // A verdict from the same model does not lift it either.
                queue.send(created(2, "b.csv")).unwrap();
                tokio::time::sleep(DAY).await;
                paths.lock().unwrap().push(step.paths());
                another_model.notify_one();
                tokio::time::sleep(DAY).await;
            })
            .await;

        assert_eq!(
            paths.into_inner().unwrap(),
            [vec!["a.csv"], vec!["a.csv", "b.csv"]]
        );
        assert_eq!(step.paths(), ["a.csv", "b.csv", "a.csv"]);
        assert_eq!(reviews.failures(), [(1, 1, None)]);
    }

    /// `step`, during whose call number `call` a model is chosen.
    struct ModelChosenDuring {
        step: Arc<Scripted>,
        call: usize,
        another_model: Arc<Notify>,
    }

    impl Step for ModelChosenDuring {
        fn decide<'a>(
            &'a self,
            input: &'a Input<'a>,
            trace: &'a (dyn Fn(Trace) + Sync),
        ) -> StepFuture<'a> {
            if self.step.asked.lock().unwrap().len() + 1 == self.call {
                self.another_model.notify_one();
            }
            self.step.decide(input, trace)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sends_a_set_again_only_once_for_a_model_chosen_during_another_review() {
        let step = Scripted::new([down(), down(), down(), refused(), refused()]);
        let another_model = Arc::new(Notify::new());
        let chooses = ModelChosenDuring {
            step: step.clone(),
            call: 3,
            another_model: another_model.clone(),
        };
        let mut reviews = Reviews::new(step.clone());
        reviews.session = reviews
            .session
            .reviewing("{}".to_string(), vec![Arc::new(chooses)])
            .trying_again_on(another_model);
        let (queue, queued) = mpsc::unbounded_channel();
        queue.send(created(1, "a.csv")).unwrap();
        queue.send(created(2, "b.csv")).unwrap();

        reviews.run(queued, tokio::time::sleep(DAY)).await;

        // A minute on, both sets are due, and the model is chosen during the first set's attempt.
        // Each then goes once under it and is refused, which nothing is left to lift.
        assert_eq!(step.paths(), ["a.csv", "b.csv", "a.csv", "a.csv", "b.csv"]);
        assert_eq!(
            reviews.failures(),
            [
                (1, 1, Some(60_000)),
                (2, 1, Some(60_000)),
                (1, 2, Some(120_000)),
                (1, 3, None),
                (2, 2, None),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn gives_a_set_that_ran_out_of_attempts_its_waits_again_with_another_model() {
        let step = Scripted::new([]);
        let another_model = Arc::new(Notify::new());
        let mut reviews = Reviews::new(step.clone());
        reviews.session = reviews.session.trying_again_on(another_model.clone());
        let (queue, queued) = mpsc::unbounded_channel();
        queue.send(created(1, "a.csv")).unwrap();

        reviews
            .run(queued, async {
                tokio::time::sleep(DAY).await;
                another_model.notify_one();
                tokio::time::sleep(Duration::from_secs(1)).await;
            })
            .await;

        // Attempts count on, so that a reader sees how many there were, and the waits start over.
        assert_eq!(reviews.failures()[6..], [(1, 7, Some(60_000))]);
        assert_eq!(
            step.since_first()[6],
            DAY,
            "goes again as soon as the model is chosen"
        );
    }

    #[test]
    fn reads_a_failed_review_from_a_log_of_before_it_could_go_again() {
        let line =
            r#"{"time":"2026-11-07T10:00:00Z","event":"review_failed","set":3,"error":"down"}"#;

        let record: Record = serde_json::from_str(line).unwrap();

        let last = Event::ReviewFailed {
            set: 3,
            error: "down".to_string(),
            attempt: 1,
            retry_in_ms: None,
        };
        assert_eq!(record.event, last);
        assert_eq!(last.decided_set(), None);
        assert_eq!(
            serde_json::to_value(&last).unwrap(),
            json!({ "event": "review_failed", "set": 3, "error": "down", "attempt": 1 })
        );
        assert_eq!(
            serde_json::to_value(Event::ReviewFailed {
                set: 3,
                error: "down".to_string(),
                attempt: 2,
                retry_in_ms: Some(120_000),
            })
            .unwrap(),
            json!({
                "event": "review_failed",
                "set": 3,
                "error": "down",
                "attempt": 2,
                "retry_in_ms": 120_000
            })
        );
    }
}
