use anyhow::Result;
use fair_witness_core::project::{DIR_NAME, IGNORE_FILE, Project};

use crate::setup::model::{self, ModelArgs};
use crate::setup::{damap, dmp};

/// Sets up `project`, offering the DAMAP URL and login of the project around it, if any.
pub async fn run(
    around: Option<&Project>,
    project: Project,
    url: Option<String>,
    model: ModelArgs,
) -> Result<()> {
    let damap = damap::setup(around, &project, url).await?;
    dmp::choose(&project, &damap).await?;
    model::setup(&project, model).await?;
    println!("Run `fair-witness run` to start watching this folder.");
    println!(
        "Edit {} to choose which paths to ignore.",
        project.root().join(DIR_NAME).join(IGNORE_FILE).display()
    );
    Ok(())
}
