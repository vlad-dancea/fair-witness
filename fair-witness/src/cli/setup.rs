use anyhow::{Context, Result};
use dialoguer::FuzzySelect;

use super::connect;
use super::model::{self, ModelArgs};
use crate::config::Config;
use crate::damap::{Damap, DmpListItem};

pub fn run(url: Option<String>, model: ModelArgs) -> Result<()> {
    let damap = connect::setup(url)?;
    choose_dmp(&damap)?;
    model::setup(model)?;
    println!("Run `fair-witness run` to start watching this folder.");
    Ok(())
}

fn choose_dmp(damap: &Damap) -> Result<()> {
    let mut config = Config::load()?.context("logging in did not save the config")?;
    let dmps = damap.list_dmps()?;
    if dmps.is_empty() {
        println!(
            "Logged in. You have no DMPs yet; create one at {}, then run `fair-witness setup` again.",
            damap.url()
        );
        config.damap.dmp_id = None;
        return config.save();
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
    config.save()
}

fn label(dmp: &DmpListItem) -> String {
    let name = dmp.name().unwrap_or("(untitled)");
    match dmp.modified.as_deref().and_then(|date| date.get(..10)) {
        Some(day) => format!("#{} {name}, changed {day}", dmp.id),
        None => format!("#{} {name}", dmp.id),
    }
}
