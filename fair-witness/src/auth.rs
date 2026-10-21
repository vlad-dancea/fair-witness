use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use dialoguer::{Input, Password};
use reqwest::blocking::Client;
use serde::Deserialize;

const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct Discovery {
    token_endpoint: String,
    device_authorization_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OAuthError {
    error: String,
    error_description: Option<String>,
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.error_description {
            Some(description) => write!(f, "{description} ({})", self.error),
            None => write!(f, "{}", self.error),
        }
    }
}

pub struct Oidc {
    http: Client,
    client_id: String,
    scope: String,
    token_endpoint: String,
    device_endpoint: Option<String>,
}

impl Oidc {
    pub fn discover(http: Client, issuer: &str, client_id: &str, scope: &str) -> Result<Self> {
        let url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let discovery: Discovery = http
            .get(&url)
            .send()
            .and_then(|r| r.error_for_status())
            .with_context(|| format!("could not reach the login server at {issuer}"))?
            .json()
            .with_context(|| format!("unexpected OpenID configuration at {url}"))?;
        Ok(Self {
            http,
            client_id: client_id.to_string(),
            scope: scope.to_string(),
            token_endpoint: discovery.token_endpoint,
            device_endpoint: discovery.device_authorization_endpoint,
        })
    }

    pub fn refresh(&self, refresh_token: &str) -> Result<Tokens> {
        self.token_request(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])?
        .map_err(|e| anyhow::anyhow!("session could not be refreshed: {e}"))
    }

    pub fn login(&self) -> Result<Tokens> {
        if let Some(endpoint) = &self.device_endpoint {
            match self.start_device_flow(endpoint)? {
                Ok(authorization) => return self.finish_device_flow(authorization),
                Err(e) if e.error == "unauthorized_client" => {
                    println!(
                        "Browser login is not enabled for Keycloak client \"{}\"; \
                         using username and password instead.",
                        self.client_id
                    );
                }
                Err(e) => bail!("login failed: {e}"),
            }
        }
        self.password_login()
    }

    fn start_device_flow(&self, endpoint: &str) -> Result<Result<DeviceAuthorization, OAuthError>> {
        let response = self
            .http
            .post(endpoint)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("scope", self.scope.as_str()),
            ])
            .send()
            .context("could not start the login")?;
        if response.status().is_success() {
            Ok(Ok(response.json()?))
        } else {
            Ok(Err(response.json()?))
        }
    }

    fn finish_device_flow(&self, authorization: DeviceAuthorization) -> Result<Tokens> {
        let link = authorization
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&authorization.verification_uri);
        println!("To log in, open {link}");
        println!("and confirm the code {}", authorization.user_code);
        let _ = open::that(link);

        let deadline = Instant::now() + Duration::from_secs(authorization.expires_in);
        let mut interval = Duration::from_secs(authorization.interval.unwrap_or(5));
        while Instant::now() < deadline {
            thread::sleep(interval);
            let result = self.token_request(&[
                ("grant_type", DEVICE_CODE_GRANT),
                ("device_code", &authorization.device_code),
            ])?;
            match result {
                Ok(tokens) => return Ok(tokens),
                Err(e) if e.error == "authorization_pending" => {}
                Err(e) if e.error == "slow_down" => interval += Duration::from_secs(5),
                Err(e) => bail!("login failed: {e}"),
            }
        }
        bail!("login timed out; please try again")
    }

    fn password_login(&self) -> Result<Tokens> {
        let username: String = Input::new().with_prompt("Username").interact_text()?;
        let password = Password::new().with_prompt("Password").interact()?;
        self.token_request(&[
            ("grant_type", "password"),
            ("username", &username),
            ("password", &password),
        ])?
        .map_err(|e| anyhow::anyhow!("login failed: {e}"))
    }

    fn token_request(&self, params: &[(&str, &str)]) -> Result<Result<Tokens, OAuthError>> {
        let mut form = vec![
            ("client_id", self.client_id.as_str()),
            ("scope", self.scope.as_str()),
        ];
        form.extend_from_slice(params);
        let response = self
            .http
            .post(&self.token_endpoint)
            .form(&form)
            .send()
            .context("could not reach the login server")?;
        if response.status().is_success() {
            Ok(Ok(response.json().context("unexpected token response")?))
        } else {
            Ok(Err(response
                .json()
                .context("unexpected error from the login server")?))
        }
    }
}
