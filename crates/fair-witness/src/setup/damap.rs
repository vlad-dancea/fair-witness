use anyhow::Result;
use dialoguer::{Confirm, Input, Password};
use fair_witness_core::auth::{Notices, Prompts};
use fair_witness_core::damap::{Damap, InstanceConfig, Login, log_in, reach};
use fair_witness_core::net::{same_url, web_url};
use fair_witness_core::project::{Config, DamapConfig, Project};

const DEFAULT_URL: &str = "https://damap.it.tuwien.ac.at";

/// Asks for what the login needs on the terminal.
pub struct TerminalPrompts;

impl Notices for TerminalPrompts {
    fn notice(&self, message: &str) {
        println!("{message}");
    }
}

impl Prompts for TerminalPrompts {
    fn confirm_code(&self, link: &str, code: &str) {
        println!("To log in, open {link}");
        println!("and confirm the code {code}");
        let _ = open::that(link);
    }

    fn username_and_password(&self) -> Result<(String, String)> {
        let username = Input::new().with_prompt("Username").interact_text()?;
        let password = Password::new().with_prompt("Password").interact()?;
        Ok((username, password))
    }

    fn stay_logged_in(&self) -> Result<bool> {
        Ok(Confirm::new()
            .with_prompt("Stay logged in?")
            .default(true)
            .interact()?)
    }
}

/// Logs `project` in to the DAMAP at `url`, or the one asked for, offering the DAMAP and login
/// of the project `around` it. Gives back the settings for `setup` to save once it has them all:
/// those `project` has, for that DAMAP, and with its DMP only if it is the same DAMAP.
pub async fn setup(
    around: Option<&Project>,
    project: &Project,
    url: Option<String>,
    prompts: &dyn Prompts,
) -> Result<(Damap, Config)> {
    let previous = project.config()?;
    let previous_url = match around {
        Some(around) => around.config()?.map(|c| c.damap.frontend_url),
        None => None,
    };
    let (damap, instance) = match url {
        Some(url) => reach(&web_url(&url)?).await?,
        None => ask_for_damap(previous_url).await?,
    };
    let login = match around {
        Some(around) => around.credentials()?.damap,
        None => None,
    };
    let damap = log_in(
        project,
        damap,
        &instance,
        login.as_ref(),
        Login::AskToStay(prompts),
    )
    .await?;
    // Another DAMAP numbers its DMPs on its own, so the one chosen before would be another
    // project's.
    let dmp_id = previous
        .as_ref()
        .filter(|previous| same_url(&previous.damap.frontend_url, damap.url()))
        .and_then(|previous| previous.damap.dmp_id);
    let config = Config {
        damap: DamapConfig {
            frontend_url: damap.url().to_string(),
            dmp_id,
        },
        ai: previous.and_then(|previous| previous.ai),
    };
    Ok((damap, config))
}

async fn ask_for_damap(previous: Option<String>) -> Result<(Damap, InstanceConfig)> {
    let mut default = previous.unwrap_or_else(|| DEFAULT_URL.to_string());
    loop {
        let input: String = Input::new()
            .with_prompt("DAMAP URL (as opened in the browser)")
            .default(default.clone())
            .interact_text()?;
        let url = match web_url(&input) {
            Ok(url) => url,
            Err(e) => {
                println!("{e}. Try again, or press Ctrl+C to stop.");
                continue;
            }
        };
        let reached = async {
            let damap = Damap::new(&url)?;
            let instance = damap.instance_config().await?;
            anyhow::Ok((damap, instance))
        };
        match reached.await {
            Ok(reached) => return Ok(reached),
            Err(e) => println!("{e:#}. Try again, or press Ctrl+C to stop."),
        }
        default = url;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use fair_witness_core::model::{Effort, ModelChoice};
    use fair_witness_core::project::AiConfig;
    use fair_witness_core::testing::fake_model;
    use serde_json::json;

    use super::*;

    /// Logs in as `user`, and would stay logged in.
    struct Person;

    impl Notices for Person {
        fn notice(&self, _: &str) {}
    }

    impl Prompts for Person {
        fn confirm_code(&self, _: &str, _: &str) {}

        fn username_and_password(&self) -> Result<(String, String)> {
            Ok(("user".to_string(), "user".to_string()))
        }

        fn stay_logged_in(&self) -> Result<bool> {
            Ok(true)
        }
    }

    /// A DAMAP whose login server takes one username and password.
    fn damap() -> String {
        let (token_server, _) = fake_model(vec![json!({
            "access_token": "access",
            "refresh_token": "refresh",
        })]);
        let (issuer, _) = fake_model(vec![json!({
            "token_endpoint": format!("{token_server}/token"),
        })]);
        let (damap, _) = fake_model(vec![json!({
            "issuer": issuer,
            "clientID": "damap",
            "scope": "openid",
        })]);
        damap
    }

    #[tokio::test]
    async fn keeps_the_dmp_only_for_the_same_damap_and_saves_nothing_but_the_login() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        let (same, other) = (damap(), damap());
        let ai = AiConfig {
            url: "https://api.example.org/v1".to_string(),
            choice: ModelChoice {
                model: "small".to_string(),
                effort: Effort::default(),
            },
        };
        project
            .save_config(&Config {
                damap: DamapConfig {
                    frontend_url: same.clone(),
                    dmp_id: Some(3),
                },
                ai: Some(ai),
            })
            .unwrap();
        let path = root.path().join(".fair-witness/config.toml");
        let saved = format!(
            "# Shared with the lab\n{}",
            fs::read_to_string(&path).unwrap()
        );
        fs::write(&path, &saved).unwrap();

        // Each stops after the login, as `setup` does on Ctrl+C at the DMP.
        let mut chosen = Vec::new();
        for url in [&same, &other] {
            let (_, config) = setup(Some(&project), &project, Some(url.clone()), &Person)
                .await
                .unwrap();
            let login = project.credentials().unwrap().damap.unwrap();
            let model = config.ai.map(|ai| ai.choice.model);
            chosen.push((
                config.damap.frontend_url,
                config.damap.dmp_id,
                model,
                login.url,
            ));
        }

        let model = Some("small".to_string());
        assert_eq!(
            chosen,
            [
                (same.clone(), Some(3), model.clone(), same),
                (other.clone(), None, model, other),
            ]
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);
    }
}
