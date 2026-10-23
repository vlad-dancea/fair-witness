use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Result;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::project::{self, Project};
use crate::review::{self, Step, Trace, Verdict};
use crate::watcher::{self, Change, FolderWatcher};

/// Something that happened while watching, with the number of the change set it belongs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A change set, numbered from 1 in the order seen.
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
    /// Watching failed after change set `set`, or before the first one when it is 0.
    WatchErrors {
        set: usize,
        errors: Vec<String>,
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
    review: Option<Review>,
    log: Option<PathBuf>,
}

struct Review {
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
            review: Some(Review { dmp, steps }),
            ..self
        }
    }

    /// Also writes every event to a new file at `path`, as a line of JSON.
    pub fn logging_to(self, path: PathBuf) -> Self {
        Self {
            log: Some(path),
            ..self
        }
    }

    /// Starts watching. The session runs until the receiver is dropped, and drops the review
    /// in progress then.
    pub fn start(self) -> Result<UnboundedReceiver<Record>> {
        let watcher = FolderWatcher::new(self.project.root(), self.debounce)?;
        let log = self
            .log
            .as_deref()
            .map(project::create_private)
            .transpose()?;
        let (sender, received) = mpsc::unbounded_channel();
        let events = Events {
            sender,
            log: Mutex::new(log),
        };
        tokio::spawn(async move {
            tokio::select! {
                () = self.watch(watcher, &events) => {}
                () = events.sender.closed() => {}
            }
        });
        Ok(received)
    }

    async fn watch(&self, mut watcher: FolderWatcher, events: &Events) {
        let mut set = 0;
        while let Some(batch) = watcher.next().await {
            let changes = match batch {
                Ok(changes) if changes.is_empty() => continue,
                Ok(changes) => changes,
                Err(errors) => {
                    let errors = errors.iter().map(ToString::to_string).collect();
                    events.send(Event::WatchErrors { set, errors });
                    continue;
                }
            };
            set += 1;
            events.send(Event::Changes {
                set,
                changes: changes.clone(),
            });
            if let Some(review) = &self.review {
                events.send(self.review(review, set, &changes, events).await);
            }
        }
    }

    // Reviews run one after another, so a review that newer changes make moot still finishes.
    // Racing it against the watcher in `watch` would let it be cancelled instead.
    async fn review(
        &self,
        review: &Review,
        set: usize,
        changes: &[Change],
        events: &Events,
    ) -> Event {
        events.send(Event::Reviewing { set });
        let trace = |trace| events.send(Event::Trace { set, trace });
        let root = self.project.root();
        match review::review(&review.steps, root, &review.dmp, changes, &trace).await {
            Ok(verdict) => Event::Verdict { set, verdict },
            Err(e) => Event::ReviewFailed {
                set,
                error: format!("{e:#}"),
            },
        }
    }
}

/// Stamps each event with the time, logs it and passes it on.
struct Events {
    sender: UnboundedSender<Record>,
    log: Mutex<Option<File>>,
}

impl Events {
    fn send(&self, event: Event) {
        let record = Record {
            time: Timestamp::now(),
            event,
        };
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(file) = log.as_mut() {
            let written = serde_json::to_vec(&record)
                .map_err(std::io::Error::from)
                .and_then(|mut line| {
                    line.push(b'\n');
                    file.write_all(&line)
                });
            // A gap in the middle of the log would read as events that never happened.
            if let Err(e) = written {
                tracing::error!("stopped writing the event log: {e}");
                *log = None;
            }
        }
        let _ = self.sender.send(record);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;
    use crate::project::AiConfig;
    use crate::review::Reviewer;
    use crate::testing::{fake_model, tool_call};

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

    fn describe(event: &Event) -> String {
        match event {
            Event::Changes { set, changes } => format!("#{set} changes {changes:?}"),
            Event::Reviewing { set } => format!("#{set} reviewing"),
            Event::Trace { set, trace } => match trace {
                Trace::Sent { role, .. } => format!("#{set} sent {role}"),
                Trace::Request { turn, .. } => format!("#{set} request {turn}"),
                Trace::Reply { turn, calls, .. } => format!("#{set} reply {turn}: {}", calls[0].0),
                Trace::ToolResult { name, output } => format!("#{set} {name}: {output}"),
                Trace::Failed(e) => format!("#{set} failed: {e}"),
            },
            Event::Verdict { set, verdict } => format!(
                "#{set} verdict: contradicts {} ({:?}): {}",
                verdict.contradicts, verdict.dmp_field, verdict.explanation
            ),
            Event::ReviewFailed { set, error } => format!("#{set} review failed: {error}"),
            Event::WatchErrors { set, errors } => format!("#{set} watch errors: {errors:?}"),
        }
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
        let reviewer = Reviewer::new(
            &AiConfig {
                url: endpoint,
                model: "test".to_string(),
                effort: None,
            },
            None,
        );
        let project = Project::new(root.path().to_path_buf());
        let log = project.run_log().unwrap().events;
        let mut events = Session::new(project)
            .debounce(Duration::from_millis(50))
            .reviewing("{}".to_string(), vec![Arc::new(reviewer)])
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
                "#1 sent system",
                "#1 sent user",
                "#1 request 1",
                "#1 reply 1: report_verdict",
                "#1 verdict: contradicts false (None): Notes are not data.",
                r#"#2 changes [Created("patients.csv")]"#,
                "#2 reviewing",
                "#2 sent system",
                "#2 sent user",
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
    }
}
