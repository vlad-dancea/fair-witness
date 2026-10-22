use anyhow::Result;
use fair_witness_core::project::Project;

pub fn run(project: Option<&Project>) -> Result<()> {
    let Some(project) = project else {
        println!("Not logged in.");
        return Ok(());
    };
    let mut credentials = project.credentials()?;
    if credentials.refresh_token.take().is_some() {
        project.save_credentials(&credentials)?;
        println!("Logged out.");
    } else {
        println!("Not logged in.");
    }
    Ok(())
}
