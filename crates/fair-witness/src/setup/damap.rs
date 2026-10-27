use anyhow::Result;
use dialoguer::{Confirm, Input, Password};
use fair_witness_core::auth::Prompts;
use fair_witness_core::damap::{Damap, InstanceConfig, discover, reach, save_session};
use fair_witness_core::net::web_url;
use fair_witness_core::project::Project;

const DEFAULT_URL: &str = "https://damap.it.tuwien.ac.at";

/// Asks for what the login needs on the terminal.
pub struct TerminalPrompts;

impl Prompts for TerminalPrompts {
    fn notice(&self, message: &str) {
        println!("{message}");
    }

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
}

pub async fn setup(
    around: Option<&Project>,
    project: &Project,
    url: Option<String>,
) -> Result<Damap> {
    let previous_url = match around {
        Some(around) => around.config()?.map(|c| c.damap.frontend_url),
        None => None,
    };
    let (damap, instance) = match url {
        Some(url) => reach(&web_url(&url)?).await?,
        None => ask_for_damap(previous_url).await?,
    };
    let oidc = discover(&damap, &instance, &TerminalPrompts).await?;
    let login = match around {
        Some(around) => around.credentials()?.damap,
        None => None,
    };
    let session = match login
        .as_ref()
        .and_then(|login| login.refresh_token_for(damap.url(), oidc.issuer()).ok())
    {
        Some(token) => oidc.refresh(token).await.ok(),
        None => None,
    };
    let tokens = match session {
        Some(tokens)
            if Confirm::new()
                .with_prompt("Stay logged in?")
                .default(true)
                .interact()? =>
        {
            tokens
        }
        _ => oidc.login(&TerminalPrompts).await?,
    };
    project.create()?;
    save_session(project, damap, &oidc, tokens, &TerminalPrompts)
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
