use anyhow::Result;
use dialoguer::FuzzySelect;
use fair_witness_core::damap::{Damap, DmpListItem};
use fair_witness_core::project::DamapConfig;

/// Asks for the DMP in `damap` that describes the folder, offering the one in `config`.
pub async fn choose(damap: &Damap, config: &mut DamapConfig) -> Result<()> {
    let dmps = damap.list_dmps().await?;
    if dmps.is_empty() {
        println!(
            "Logged in. You have no DMPs yet; create one at {}, then run `fair-witness setup` again.",
            damap.url()
        );
        config.dmp_id = None;
        return Ok(());
    }
    let labels: Vec<String> = dmps.iter().map(label).collect();
    let default = config
        .dmp_id
        .and_then(|id| dmps.iter().position(|dmp| dmp.id == id))
        .unwrap_or(0);
    let index = FuzzySelect::new()
        .with_prompt("DMP for this folder (type to filter)")
        .items(&labels)
        .default(default)
        .interact()?;
    config.dmp_id = Some(dmps[index].id);
    Ok(())
}

fn label(dmp: &DmpListItem) -> String {
    let name = dmp.name().unwrap_or("(untitled)");
    match dmp.modified.as_deref().and_then(|date| date.get(..10)) {
        Some(day) => format!("#{} {name}, changed {day}", dmp.id),
        None => format!("#{} {name}", dmp.id),
    }
}
