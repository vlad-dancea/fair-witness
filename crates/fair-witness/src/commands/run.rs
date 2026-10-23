use std::sync::Arc;

use anyhow::{Context, Result, bail};
use fair_witness_core::damap;
use fair_witness_core::project::Project;
use fair_witness_core::review::Reviewer;
use fair_witness_core::session::Session;
use tokio::sync::mpsc;

use crate::notifications;
use crate::setup::damap::{TerminalPrompts, http_client};
use crate::tui::app::{Screen, Update};
use crate::tui::model_panel::ModelSettings;

pub async fn run(project: &Project, dmp: Option<i64>) -> Result<()> {
    let damap = damap::resume(project, http_client()?, &TerminalPrompts).await?;

    let config = project.config()?.context("this folder is not set up yet")?;
    let mut session = Session::new(project.clone());
    let (settings, status) = match (dmp.or(config.damap.dmp_id), config.ai) {
        (Some(id), Some(ai)) => {
            let api_key = project.credentials()?.api_key;
            let reviewer = Arc::new(Reviewer::new(&ai, api_key.clone()));
            let name = damap
                .list_dmps()
                .await?
                .into_iter()
                .find(|listed| listed.id == id)
                .and_then(|listed| listed.name().map(str::to_string))
                .unwrap_or_else(|| "(untitled)".to_string());
            let madmp = damap.madmp(id).await?;
            if config.damap.dmp_id == Some(id) {
                project.save_madmp(&config.damap.frontend_url, id, &madmp)?;
            }
            let dmp = serde_json::to_string_pretty(&madmp)?;
            session = session.reviewing(dmp, vec![reviewer.clone()]);
            let status = vec![format!("Comparing changes with project #{id} - {name}.")];
            let settings = ModelSettings {
                project: project.clone(),
                reviewer,
                url: ai.url,
                api_key,
            };
            (Some(settings), status)
        }
        (Some(_), None) if dmp.is_some() => {
            bail!("no AI model to check with; choose one with `fair-witness setup`")
        }
        (None, Some(_)) => (
            None,
            vec![
                "No DMP chosen, so changes are not checked; choose one with `fair-witness setup`."
                    .to_string(),
            ],
        ),
        _ => (None, vec!["Changes are not checked.".to_string()]),
    };

    let mut events = session.start()?;
    let (updates, received) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            notifications::show(&event, &updates);
            if updates.send(Update::Session(event)).is_err() {
                return;
            }
        }
    });
    Screen::new(project.root(), status, settings)
        .run(received)
        .await
}
