use anyhow::{Context, Result};
use dialoguer::FuzzySelect;

use super::connect;
use super::model::{self, ModelArgs};
use crate::damap::{Damap, DmpListItem};
use crate::project::Project;

/// Sets up `project`, offering the DAMAP URL and login of the project around it, if any.
pub fn run(
    around: Option<&Project>,
    project: Project,
    url: Option<String>,
    model: ModelArgs,
) -> Result<()> {
    let damap = connect::setup(around, &project, url)?;
    choose_dmp(&project, &damap)?;
    model::setup(&project, model)?;
    println!("Run `fair-witness run` to start watching this folder.");
    Ok(())
}

fn choose_dmp(project: &Project, damap: &Damap) -> Result<()> {
    let mut config = project
        .config()?
        .context("logging in did not save the config")?;
    let dmps = damap.list_dmps()?;
    if dmps.is_empty() {
        println!(
            "Logged in. You have no DMPs yet; create one at {}, then run `fair-witness setup` again.",
            damap.url()
        );
        config.damap.dmp_id = None;
        return project.save_config(&config);
    }
    let labels: Vec<String> = dmps.iter().map(label).collect();
    let default = config
        .damap
        .dmp_id
        .and_then(|id| dmps.iter().position(|dmp| dmp.id == id))
        .unwrap_or(0);
    let index = FuzzySelect::new()
        .with_prompt("DMP for this folder (type to filter)")
        .items(&labels)
        .default(default)
        .interact()?;
    config.damap.dmp_id = Some(dmps[index].id);
    project.save_config(&config)
}

fn label(dmp: &DmpListItem) -> String {
    let name = dmp.name().unwrap_or("(untitled)");
    match dmp.modified.as_deref().and_then(|date| date.get(..10)) {
        Some(day) => format!("#{} {name}, changed {day}", dmp.id),
        None => format!("#{} {name}", dmp.id),
    }
}
