use anyhow::{Result, bail};
use dialoguer::{Confirm, Input, Password};
use fair_witness_core::auth::Prompts;
use fair_witness_core::damap::{Damap, InstanceConfig, discover, reach, save_session};
use fair_witness_core::project::Project;
use reqwest::Url;

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
        None => ask_for_damap(previous_url.clone()).await?,
    };
    let oidc = discover(&damap, &instance, &TerminalPrompts).await?;
    let refresh_token = match around {
        Some(around) => around.credentials()?.refresh_token,
        None => None,
    };
    let session = match refresh_token {
        Some(token) if previous_url.as_deref() == Some(damap.url()) => {
            oidc.refresh(&token).await.ok()
        }
        _ => None,
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
    save_session(project, damap, tokens, &TerminalPrompts)
}

/// Reads a web address as typed: `https://` is assumed when no scheme is given.
pub fn web_url(input: &str) -> Result<String> {
    let input = input.trim();
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else {
        format!("https://{input}")
    };
    let looks_like_a_site = |url: &Url| {
        matches!(url.scheme(), "http" | "https")
            && url
                .host_str()
                .is_some_and(|host| host.contains('.') || host == "localhost")
    };
    match Url::parse(&with_scheme) {
        Ok(url) if looks_like_a_site(&url) => Ok(with_scheme),
        _ => bail!("{input} is not a web address like https://example.org"),
    }
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
    use super::web_url;

    #[test]
    fn reads_web_addresses_as_typed() {
        assert_eq!(
            web_url("damap.it.tuwien.ac.at").unwrap(),
            "https://damap.it.tuwien.ac.at"
        );
        assert_eq!(
            web_url(" http://localhost:8085 ").unwrap(),
            "http://localhost:8085"
        );
        assert_eq!(
            web_url("https://aqueduct.ai.datalab.tuwien.ac.at/v1").unwrap(),
            "https://aqueduct.ai.datalab.tuwien.ac.at/v1"
        );
    }

    #[test]
    fn rejects_what_is_not_a_web_address() {
        for input in ["user", "", "ftp://example.org", "https://"] {
            assert!(web_url(input).is_err(), "{input}");
        }
    }
}
