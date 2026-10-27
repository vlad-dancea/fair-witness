use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Result;
use jiff::{Timestamp, Unit};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::project::{self, Project};
use crate::review::{self, Step, Trace, Verdict};
use crate::watcher::{self, Change, Filter, FolderWatcher, WatchError};

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
    ReviewFailed {
        set: usize,
        error: String,
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

    /// Also adds every event to the file at `path`, as a line of JSON.
    pub fn logging_to(self, path: PathBuf) -> Self {
        Self {
            log: Some(path),
            ..self
        }
    }

    /// Starts watching. The session runs until the receiver is dropped or watching stops, and
    /// drops the review in progress then.
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

    // Reviews run one after another, in the order of their sets, so a review that newer changes
    // make moot still finishes, and the sets that came meanwhile wait in `queued`.
    async fn review_queued(
        &self,
        mut queued: UnboundedReceiver<(usize, Vec<Change>)>,
        filter: &Filter,
        events: &Events,
    ) {
        while let Some((set, changes)) = queued.recv().await {
            if let Some(settings) = &self.review {
                events.send(
                    self.review_set(settings, set, &changes, filter, events)
                        .await,
                );
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
    ) -> Event {
        events.send(Event::Reviewing { set });
        let trace = |trace| events.send(Event::Trace { set, trace });
        let root = self.project.root();
        match review::run_steps(
            &settings.steps,
            root,
            filter,
            &settings.dmp,
            changes,
            &trace,
        )
        .await
        {
            Ok(verdict) => Event::Verdict { set, verdict },
            Err(e) => Event::ReviewFailed {
                set,
                error: format!("{e:#}"),
            },
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
    use std::fs;

    use serde_json::json;

    use super::*;
    use crate::model::{Effort, ModelChoice};
    use crate::project::AiConfig;
    use crate::review::{Input, ModelStep, StepFuture};
    use crate::testing::{Reply, fake_model, held, no_answer, tool_call};

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
            Event::ReviewFailed { set, error } => format!("#{set} review failed: {error}"),
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
            Duration::from_secs(60),
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
                "#1 failed: the API did not answer within 1 s",
                "#1 review failed: the model test failed: the API did not answer within 1 s",
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
}
