use anyhow::Result;
use fair_witness_core::project::{DIR_NAME, IGNORE_FILE, Project};

use crate::setup::damap;
use crate::setup::dmp;
use crate::setup::model::{self, ModelArgs};
use crate::terminal::TerminalPrompts;

/// Sets up `project`, offering the DAMAP URL and login of the project around it, if any.
pub async fn run(
    around: Option<&Project>,
    project: Project,
    url: Option<String>,
    model: ModelArgs,
) -> Result<()> {
    let (damap, mut config) = damap::setup(around, &project, url, &TerminalPrompts).await?;
    dmp::choose(&damap, &mut config.damap).await?;
    config.ai = Some(model::setup(&project, config.ai.take(), model).await?);
    // Saved once every question is answered, so that a `setup` stopped early leaves the
    // settings as they were.
    project.create()?;
    project.save_config(&config)?;
    println!("Run `fair-witness run` to start watching this folder.");
    println!(
        "Edit {} to choose which paths to ignore.",
        project.root().join(DIR_NAME).join(IGNORE_FILE).display()
    );
    Ok(())
}
