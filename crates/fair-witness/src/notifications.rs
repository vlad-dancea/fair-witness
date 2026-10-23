use fair_witness_core::session::Event;
use notify_rust::Notification;
use tokio::sync::mpsc::UnboundedSender;

use crate::tui::app::Update;

/// Shows a desktop notification for a contradiction or a failed review, and tells the screen
/// when that fails.
pub fn show(event: &Event, updates: &UnboundedSender<Update>) {
    let (summary, body) = match event {
        Event::Verdict { set, verdict } if verdict.contradicts => {
            let field = verdict
                .dmp_field
                .as_ref()
                .map(|field| format!(" ({field})"))
                .unwrap_or_default();
            (
                format!("#{set} contradicts the DMP{field}"),
                verdict.explanation.clone(),
            )
        }
        Event::ReviewFailed { set, error } => {
            (format!("Could not check change set #{set}"), error.clone())
        }
        _ => return,
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
