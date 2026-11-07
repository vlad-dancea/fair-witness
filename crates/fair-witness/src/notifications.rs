use fair_witness_core::session::Event;
use notify_rust::Notification;
use tokio::sync::mpsc::UnboundedSender;

use crate::tui::app::{Update, next_attempt};

/// Shows a desktop notification for a contradiction or a change set whose review failed, and tells
/// the screen when that fails. `comes_back` says whether a failed set returns at the next start.
pub fn show(event: &Event, comes_back: bool, updates: &UnboundedSender<Update>) {
    let Some((summary, body)) = content(event, comes_back) else {
        return;
    };
    let updates = updates.clone();
    tokio::task::spawn_blocking(move || {
        let shown = Notification::new()
            .appname("fair-witness")
            .summary(&summary)
            .body(&body)
            .show();
        if let Err(e) = shown {
            tracing::warn!("could not show a notification: {e}");
            let _ = updates.send(Update::NotifyFailed(e.to_string()));
        }
    });
}

/// The summary and body of the notification for `event`, if it gets one.
fn content(event: &Event, comes_back: bool) -> Option<(String, String)> {
    match event {
        Event::Verdict { set, verdict } if verdict.contradicts => {
            let field = verdict
                .dmp_field
                .as_ref()
                .map(|field| format!(" ({field})"))
                .unwrap_or_default();
            Some((
                format!("#{set} contradicts the DMP{field}"),
                verdict.explanation.clone(),
            ))
        }
        // Only for its first attempt, so that one set gives one notification. Notification daemons
        // keep a history, so the rest of an error stays in the run's log.
        Event::ReviewFailed {
            set,
            error,
            attempt: 1,
            retry_in_ms,
        } => Some((
            format!(
                "Could not check change set #{set}, {}",
                next_attempt(*retry_in_ms, comes_back)
            ),
            error.lines().next().unwrap_or_default().to_string(),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use fair_witness_core::model::{Effort, ModelChoice};
    use fair_witness_core::project::{AiConfig, Project};
    use fair_witness_core::review::{ModelStep, Trace};
    use fair_witness_core::session::Session;
    use fair_witness_core::testing::{failure, fake_model};

    use super::*;

    #[tokio::test]
    async fn tells_of_a_failed_review_in_one_line_without_the_api_key() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, _server) = fake_model(vec![failure(
            "401 Unauthorized",
            "Incorrect API key provided: sk-test.\nYou can find your API key at \
             https://example.org/keys.",
        )]);
        let ai = AiConfig {
            url: endpoint,
            choice: ModelChoice {
                model: "test".to_string(),
                effort: Effort::default(),
            },
        };
        let reviewer = ModelStep::new(&ai, Some("sk-test".to_string())).unwrap();
        let mut records = Session::new(Project::new(root.path().to_path_buf()))
            .debounce(Duration::from_millis(50))
            .reviewing("{}".to_string(), vec![Arc::new(reviewer)])
            .start()
            .unwrap();
        fs::write(root.path().join("notes.txt"), "Ask about consent").unwrap();

        let mut failed = None;
        let error = loop {
            let record = tokio::time::timeout(Duration::from_secs(10), records.recv())
                .await
                .expect("no review failed in time")
                .expect("the session stopped");
            match record.event {
                Event::Trace {
                    trace: Trace::Failed(why),
                    ..
                } => failed = Some(why),
                event @ Event::ReviewFailed { .. } => break event,
                _ => {}
            }
        };

        let why = "the API answered 401 Unauthorized: Incorrect API key provided: [API key]. \
                   You can find your API key at https://example.org/keys.";
        assert_eq!(failed.as_deref(), Some(why));
        assert_eq!(
            error,
            Event::ReviewFailed {
                set: 1,
                error: format!("the model test failed: {why}"),
                attempt: 1,
                retry_in_ms: None,
            }
        );
        assert_eq!(
            content(&error, false),
            Some((
                "Could not check change set #1, no more attempts in this run unless you pick another \
                 model (m)"
                    .to_string(),
                format!("the model test failed: {why}"),
            ))
        );
        assert_eq!(
            content(&error, true),
            Some((
                "Could not check change set #1, no more attempts until you pick another model (m) \
                 or start run again"
                    .to_string(),
                format!("the model test failed: {why}"),
            ))
        );
    }

    #[test]
    fn shows_only_the_first_line_of_an_error() {
        let failed = Event::ReviewFailed {
            set: 2,
            error: "the model test failed\nRequest Payload: {\"messages\": []}".to_string(),
            attempt: 1,
            retry_in_ms: Some(120_000),
        };

        assert_eq!(
            content(&failed, true),
            Some((
                "Could not check change set #2, trying again in 2 minutes".to_string(),
                "the model test failed".to_string(),
            ))
        );
    }

    #[test]
    fn notifies_once_for_a_set_however_many_attempts_fail() {
        let failed = |set, attempt, retry_in_ms| Event::ReviewFailed {
            set,
            error: "down".to_string(),
            attempt,
            retry_in_ms,
        };
        let waits = [Some(60_000), Some(120_000), Some(300_000), None];
        let shown = |set| {
            (1..)
                .zip(waits)
                .filter_map(|(attempt, wait)| content(&failed(set, attempt, wait), true))
                .count()
        };

        assert_eq!((shown(1), shown(2)), (1, 1));
    }
}
