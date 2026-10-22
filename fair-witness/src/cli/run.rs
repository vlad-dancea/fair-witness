use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread;

use anyhow::{Context, Result, bail};
use notify_rust::Notification;

use super::connect;
use super::model_panel::ModelSettings;
use super::screen::{Screen, Update};
use crate::agent::Reviewer;
use crate::config::{self, Config, Credentials};
use crate::watcher::{Change, FolderWatcher};

struct Review {
    reviewer: Arc<Reviewer>,
    dmp: String,
}

pub fn run(dmp: Option<i64>) -> Result<()> {
    let damap = connect::resume()?;
    let root = config::project_root()?.context("this folder is not set up yet")?;

    let config = Config::load()?.context("this folder is not set up yet")?;
    let (review, settings, status) = match (dmp.or(config.damap.dmp_id), config.ai) {
        (Some(id), Some(ai)) => {
            let api_key = Credentials::load()?.api_key;
            let reviewer = Arc::new(Reviewer::new(&ai, api_key.clone())?);
            let name = damap
                .list_dmps()?
                .into_iter()
                .find(|listed| listed.id == id)
                .and_then(|listed| listed.name().map(str::to_string))
                .unwrap_or_else(|| "(untitled)".to_string());
            let dmp = serde_json::to_string_pretty(&damap.madmp(id)?)?;
            config::save_madmp(&dmp)?;
            let status = vec![format!("Comparing changes with project #{id} - {name}.")];
            let settings = ModelSettings {
                reviewer: reviewer.clone(),
                url: ai.url,
                api_key,
            };
            (Some(Review { reviewer, dmp }), Some(settings), status)
        }
        (Some(_), None) if dmp.is_some() => {
            bail!("no AI model to check with; choose one with `fair-witness setup`")
        }
        (None, Some(_)) => (
            None,
            None,
            vec![
                "No DMP chosen, so changes are not checked; choose one with `fair-witness setup`."
                    .to_string(),
            ],
        ),
        _ => (None, None, vec!["Changes are not checked.".to_string()]),
    };

    let watcher = FolderWatcher::new(&root)?;
    let (updates, received) = mpsc::channel();
    let watched = root.clone();
    thread::spawn(move || watch(&watched, watcher, review.as_ref(), &updates));
    Screen::new(&root, status, settings).run(received)
}

fn watch(
    root: &Path,
    watcher: FolderWatcher,
    review: Option<&Review>,
    updates: &mpsc::Sender<Update>,
) {
    let mut set = 0;
    for batch in watcher {
        let changes: Vec<Change> = match batch {
            Ok(changes) => changes,
            Err(errors) => {
                let errors = errors.iter().map(ToString::to_string).collect();
                if updates.send(Update::WatchErrors(errors)).is_err() {
                    return;
                }
                continue;
            }
        };
        if changes.is_empty() {
            continue;
        }
        set += 1;
        if updates.send(Update::Changes(set, changes.clone())).is_err() {
            return;
        }
        if let Some(review) = review {
            if updates.send(Update::Reviewing(set)).is_err() {
                return;
            }
            let verdict = review
                .reviewer
                .review(root, &review.dmp, &changes, &mut |trace| {
                    let _ = updates.send(Update::Trace(trace));
                });
            match &verdict {
                Ok(verdict) if verdict.contradicts => {
                    let field = verdict
                        .dmp_field
                        .as_ref()
                        .map(|field| format!(" ({field})"))
                        .unwrap_or_default();
                    notify(
                        format!("#{set} contradicts the DMP{field}"),
                        verdict.explanation.clone(),
                        updates,
                    );
                }
                Ok(_) => {}
                Err(e) => notify(
                    format!("Could not check change set #{set}"),
                    format!("{e:#}"),
                    updates,
                ),
            }
            if updates.send(Update::Verdict(verdict)).is_err() {
                return;
            }
        }
    }
}

fn notify(summary: String, body: String, updates: &mpsc::Sender<Update>) {
    let updates = updates.clone();
    thread::spawn(move || {
        let shown = Notification::new()
            .appname("fair-witness")
            .summary(&summary)
            .body(&body)
            .show();
        if let Err(e) = shown {
            let _ = updates.send(Update::NotifyFailed(e.to_string()));
        }
    });
}
