use anyhow::{Result, anyhow, bail};

use super::{Damap, InstanceConfig};
use crate::auth::{Oidc, Prompts, Tokens};
use crate::project::{Config, DamapConfig, NOT_SET_UP, Project};

/// Logs in to the DAMAP `project` is set up with, with the saved session while it lasts.
pub async fn resume(project: &Project, prompts: &dyn Prompts) -> Result<Damap> {
    let Some(config) = project.config()? else {
        bail!(NOT_SET_UP);
    };
    let (damap, instance) = reach(&config.damap.frontend_url).await?;
    let oidc = discover(&damap, &instance, prompts).await?;
    let tokens = match project.credentials()?.refresh_token {
        Some(refresh_token) => match oidc.refresh(&refresh_token).await {
            Ok(tokens) => tokens,
            Err(e) => {
                prompts.notice(&format!("{e}. Please log in again."));
                oidc.login(prompts).await?
            }
        },
        None => {
            prompts.notice("Not logged in.");
            oidc.login(prompts).await?
        }
    };
    save_session(project, damap, tokens, prompts)
}

pub async fn reach(url: &str) -> Result<(Damap, InstanceConfig)> {
    let damap = Damap::new(url)
        .map_err(|e| anyhow!("{e:#}\nTo use a different URL, run `fair-witness setup`."))?;
    let instance = damap.instance_config().await.map_err(|e| {
        anyhow!("{e:#}\nIs DAMAP running? To use a different URL, run `fair-witness setup`.")
    })?;
    Ok((damap, instance))
}

/// Finds the login server of the DAMAP `instance` belongs to.
pub async fn discover(
    damap: &Damap,
    instance: &InstanceConfig,
    prompts: &dyn Prompts,
) -> Result<Oidc> {
    let title = instance.app_title.as_deref().unwrap_or("DAMAP");
    prompts.notice(&format!("Connected to {title} at {}", damap.url()));
    Oidc::discover(&instance.issuer, &instance.client_id, &instance.scope).await
}

/// Saves DAMAP's URL and the login in `project`, and logs `damap` in with `tokens`.
pub fn save_session(
    project: &Project,
    mut damap: Damap,
    tokens: Tokens,
    prompts: &dyn Prompts,
) -> Result<Damap> {
    let previous = project.config()?;
    project.save_config(&Config {
        damap: DamapConfig {
            frontend_url: damap.url().to_string(),
            dmp_id: previous.as_ref().and_then(|config| config.damap.dmp_id),
        },
        ai: previous.and_then(|config| config.ai),
    })?;
    match tokens.refresh_token {
        Some(refresh_token) => {
            let mut credentials = project.credentials()?;
            credentials.refresh_token = Some(refresh_token);
            project.save_credentials(&credentials)?
        }
        None => prompts.notice(
            "The login server issued no refresh token; you will have to log in again next time.",
        ),
    }
    damap.set_access_token(tokens.access_token);
    Ok(damap)
}
