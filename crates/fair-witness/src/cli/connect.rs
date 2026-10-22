use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use dialoguer::{Confirm, Input};
use reqwest::Url;
use reqwest::blocking::Client;

use crate::auth::{Oidc, Tokens};
use crate::damap::{Damap, InstanceConfig};
use crate::project::{Config, DamapConfig, Project};

const DEFAULT_URL: &str = "https://damap.it.tuwien.ac.at";

pub fn setup(around: Option<&Project>, project: &Project, url: Option<String>) -> Result<Damap> {
    let http = http_client()?;
    let previous_url = match around {
        Some(around) => around.config()?.map(|c| c.damap.frontend_url),
        None => None,
    };
    let (damap, instance) = match url {
        Some(url) => reach(&http, &web_url(&url)?)?,
        None => ask_for_damap(&http, previous_url.clone())?,
    };
    let oidc = discover(http, &damap, &instance)?;
    let refresh_token = match around {
        Some(around) => around.credentials()?.refresh_token,
        None => None,
    };
    let session = match refresh_token {
        Some(token) if previous_url.as_deref() == Some(damap.url()) => oidc.refresh(&token).ok(),
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
        _ => oidc.login()?,
    };
    project.create()?;
    save_session(project, damap, tokens)
}

pub fn resume(project: &Project) -> Result<Damap> {
    let Some(config) = project.config()? else {
        bail!(super::NOT_SET_UP);
    };
    let http = http_client()?;
    let (damap, instance) = reach(&http, &config.damap.frontend_url)?;
    let oidc = discover(http, &damap, &instance)?;
    let tokens = match project.credentials()?.refresh_token {
        Some(refresh_token) => match oidc.refresh(&refresh_token) {
            Ok(tokens) => tokens,
            Err(e) => {
                println!("{e}. Please log in again.");
                oidc.login()?
            }
        },
        None => {
            println!("Not logged in.");
            oidc.login()?
        }
    };
    save_session(project, damap, tokens)
}

/// Reads a web address as typed: `https://` is assumed when no scheme is given.
pub(super) fn web_url(input: &str) -> Result<String> {
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

pub(super) fn http_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("fair-witness/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()?)
}

fn discover(http: Client, damap: &Damap, instance: &InstanceConfig) -> Result<Oidc> {
    let title = instance.app_title.as_deref().unwrap_or("DAMAP");
    println!("Connected to {title} at {}", damap.url());
    Oidc::discover(http, &instance.issuer, &instance.client_id, &instance.scope)
}

fn save_session(project: &Project, mut damap: Damap, tokens: Tokens) -> Result<Damap> {
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
        None => println!(
            "The login server issued no refresh token; you will have to log in again next time."
        ),
    }
    damap.set_access_token(tokens.access_token);
    Ok(damap)
}

fn reach(http: &Client, url: &str) -> Result<(Damap, InstanceConfig)> {
    let damap = Damap::new(http.clone(), url);
    let instance = damap.instance_config().map_err(|e| {
        anyhow!("{e:#}\nIs DAMAP running? To use a different URL, run `fair-witness setup`.")
    })?;
    Ok((damap, instance))
}

fn ask_for_damap(http: &Client, previous: Option<String>) -> Result<(Damap, InstanceConfig)> {
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
        let damap = Damap::new(http.clone(), &url);
        match damap.instance_config() {
            Ok(instance) => return Ok((damap, instance)),
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
