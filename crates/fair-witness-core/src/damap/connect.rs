use anyhow::{Result, anyhow, bail};

use super::{Damap, InstanceConfig};
use crate::auth::{Oidc, Prompts, Tokens};
use crate::project::{Config, DamapConfig, DamapLogin, NOT_SET_UP, Project};

/// Logs in to the DAMAP `project` is set up with, with the saved session while it lasts.
pub async fn resume(project: &Project, prompts: &dyn Prompts) -> Result<Damap> {
    let Some(config) = project.config()? else {
        bail!(NOT_SET_UP);
    };
    let (damap, instance) = reach(&config.damap.frontend_url).await?;
    let oidc = discover(&damap, &instance, prompts).await?;
    let tokens = match project.credentials()?.damap {
        Some(login) => {
            let refreshed = match login.refresh_token_for(damap.url(), oidc.issuer()) {
                Ok(refresh_token) => oidc.refresh(refresh_token).await,
                Err(e) => Err(e),
            };
            match refreshed {
                Ok(tokens) => tokens,
                Err(e) => {
                    prompts.notice(&format!("{e}. Please log in again."));
                    oidc.login(prompts).await?
                }
            }
        }
        None => {
            prompts.notice("Not logged in.");
            oidc.login(prompts).await?
        }
    };
    save_session(project, damap, &oidc, tokens, prompts)
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

/// Saves DAMAP's URL and the login in `project`, and logs `damap` in with `tokens` from the
/// login server `oidc`.
pub fn save_session(
    project: &Project,
    mut damap: Damap,
    oidc: &Oidc,
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
    let mut credentials = project.credentials()?;
    // A login without a refresh token leaves none saved, since the one from before may be for
    // another DAMAP. A refresh always brings one, the old one when the login server issues none.
    credentials.damap = tokens.refresh_token.map(|refresh_token| DamapLogin {
        url: damap.url().to_string(),
        login_server: Some(oidc.issuer().to_string()),
        refresh_token,
    });
    project.save_credentials(&credentials)?;
    if credentials.damap.is_none() {
        prompts.notice(
            "The login server issued no refresh token; you will have to log in again next time.",
        );
    }
    damap.set_access_token(tokens.access_token);
    Ok(damap)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;
    use crate::testing::fake_model;

    /// Keeps the notices.
    #[derive(Default)]
    struct Notices(RefCell<Vec<String>>);

    impl Prompts for Notices {
        fn notice(&self, message: &str) {
            self.0.borrow_mut().push(message.to_string());
        }

        fn confirm_code(&self, _: &str, _: &str) {}

        fn username_and_password(&self) -> Result<(String, String)> {
            bail!("not asked in this test")
        }
    }

    #[tokio::test]
    async fn saves_a_login_with_its_servers_and_drops_it_after_one_without_a_refresh_token() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        let (issuer, _) = fake_model(vec![json!({
            "token_endpoint": "https://login.example.org/token",
        })]);
        let oidc = Oidc::discover(&issuer, "damap", "openid").await.unwrap();
        let notices = Notices::default();
        let log_in = |url: &str, refresh_token: Option<&str>| {
            let tokens = Tokens {
                access_token: "access".to_string(),
                refresh_token: refresh_token.map(str::to_string),
            };
            save_session(&project, Damap::new(url).unwrap(), &oidc, tokens, &notices).unwrap();
            project.credentials().unwrap().damap
        };

        let with_token = log_in("https://damap.example.org/", Some("refresh"));
        let without = log_in("https://other.example.org", None);

        assert_eq!(
            [with_token, without],
            [
                Some(DamapLogin {
                    url: "https://damap.example.org".to_string(),
                    login_server: Some(issuer),
                    refresh_token: "refresh".to_string(),
                }),
                None,
            ]
        );
        assert_eq!(
            notices.0.into_inner(),
            [
                "The login server issued no refresh token; you will have to log in again next \
                 time."
            ]
        );
    }
}
