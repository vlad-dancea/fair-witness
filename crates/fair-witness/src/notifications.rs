use fair_witness_core::session::Event;
use notify_rust::Notification;
use tokio::sync::mpsc::UnboundedSender;

use crate::tui::app::Update;

/// Shows a desktop notification for a contradiction or a failed review, and tells the screen
/// when that fails.
pub fn show(event: &Event, updates: &UnboundedSender<Update>) {
    let Some((summary, body)) = content(event) else {
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
fn content(event: &Event) -> Option<(String, String)> {
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
        // Notification daemons keep a history, so the rest of an error stays in the run's log.
        Event::ReviewFailed { set, error } => Some((
            format!("Could not check change set #{set}"),
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
            }
        );
        assert_eq!(
            content(&error),
            Some((
                "Could not check change set #1".to_string(),
                format!("the model test failed: {why}"),
            ))
        );
    }

    #[test]
    fn shows_only_the_first_line_of_an_error() {
        let failed = Event::ReviewFailed {
            set: 2,
            error: "the model test failed\nRequest Payload: {\"messages\": []}".to_string(),
        };

        assert_eq!(
            content(&failed),
            Some((
                "Could not check change set #2".to_string(),
                "the model test failed".to_string(),
            ))
        );
    }
}
