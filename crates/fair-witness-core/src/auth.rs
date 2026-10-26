use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;

use crate::net::{self, in_the_clear};

/// What logging in to DAMAP asks of the person in front of the screen.
pub trait Prompts {
    /// Tells them how the login goes, such as which DAMAP it reached.
    fn notice(&self, message: &str);
    /// Asks them to confirm `code` at `link` in the browser.
    fn confirm_code(&self, link: &str, code: &str);
    fn username_and_password(&self) -> Result<(String, String)>;
}

const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// How long a request to the login server may take.
const TIMEOUT: Duration = Duration::from_secs(15);

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

/// Says that the login server at `url` would get the login unencrypted.
fn unencrypted(url: &str) -> String {
    format!(
        "DAMAP's login server at {url} uses plain HTTP to another machine, which would send the \
         login unencrypted\nAsk the people who run this DAMAP to serve its login over https://"
    )
}

pub struct Oidc {
    /// For the token endpoint, which may be on another machine than the issuer and so need
    /// another proxy setting.
    token_http: Client,
    client_id: String,
    scope: String,
    token_endpoint: String,
    device_endpoint: Option<String>,
}

impl Oidc {
    pub async fn discover(issuer: &str, client_id: &str, scope: &str) -> Result<Self> {
        // The issuer is asked where to log in, so an answer read and changed along the way could
        // send the login anywhere.
        if in_the_clear(issuer) {
            bail!(unencrypted(issuer));
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let discovery: Discovery = net::client(issuer, TIMEOUT)?
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .with_context(|| format!("could not reach the login server at {issuer}"))?
            .json()
            .await
            .with_context(|| format!("unexpected OpenID configuration at {url}"))?;
        let endpoints = [
            Some(&discovery.token_endpoint),
            discovery.device_authorization_endpoint.as_ref(),
        ];
        if let Some(endpoint) = endpoints
            .into_iter()
            .flatten()
            .find(|url| in_the_clear(url))
        {
            bail!(unencrypted(endpoint));
        }
        Ok(Self {
            token_http: net::client(&discovery.token_endpoint, TIMEOUT)?,
            client_id: client_id.to_string(),
            scope: scope.to_string(),
            token_endpoint: discovery.token_endpoint,
            device_endpoint: discovery.device_authorization_endpoint,
        })
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens> {
        self.token_request(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .await?
        .map_err(|e| anyhow::anyhow!("session could not be refreshed: {e}"))
    }

    pub async fn login(&self, prompts: &dyn Prompts) -> Result<Tokens> {
        if let Some(endpoint) = &self.device_endpoint {
            match self.start_device_flow(endpoint).await? {
                Ok(authorization) => {
                    return self.finish_device_flow(authorization, prompts).await;
                }
                Err(e) if e.error == "unauthorized_client" => {
                    prompts.notice(&format!(
                        "Browser login is not enabled for Keycloak client \"{}\"; \
                         using username and password instead.",
                        self.client_id
                    ));
                }
                Err(e) => bail!("login failed: {e}"),
            }
        }
        self.password_login(prompts).await
    }

    async fn start_device_flow(
        &self,
        endpoint: &str,
    ) -> Result<Result<DeviceAuthorization, OAuthError>> {
        let response = net::client(endpoint, TIMEOUT)?
            .post(endpoint)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("scope", self.scope.as_str()),
            ])
            .send()
            .await
            .context("could not start the login")?;
        if response.status().is_success() {
            Ok(Ok(response.json().await?))
        } else {
            Ok(Err(response.json().await?))
        }
    }

    async fn finish_device_flow(
        &self,
        authorization: DeviceAuthorization,
        prompts: &dyn Prompts,
    ) -> Result<Tokens> {
        let link = authorization
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&authorization.verification_uri);
        // The browser would ask for the password there.
        if in_the_clear(link) {
            bail!(unencrypted(link));
        }
        prompts.confirm_code(link, &authorization.user_code);

        let deadline = Instant::now() + Duration::from_secs(authorization.expires_in);
        let mut interval = Duration::from_secs(authorization.interval.unwrap_or(5));
        while Instant::now() < deadline {
            tokio::time::sleep(interval).await;
            let result = self
                .token_request(&[
                    ("grant_type", DEVICE_CODE_GRANT),
                    ("device_code", &authorization.device_code),
                ])
                .await?;
            match result {
                Ok(tokens) => return Ok(tokens),
                Err(e) if e.error == "authorization_pending" => {}
                Err(e) if e.error == "slow_down" => interval += Duration::from_secs(5),
                Err(e) => bail!("login failed: {e}"),
            }
        }
        bail!("login timed out; please try again")
    }

    async fn password_login(&self, prompts: &dyn Prompts) -> Result<Tokens> {
        let (username, password) = prompts.username_and_password()?;
        self.token_request(&[
            ("grant_type", "password"),
            ("username", &username),
            ("password", &password),
        ])
        .await?
        .map_err(|e| anyhow::anyhow!("login failed: {e}"))
    }

    async fn token_request(&self, params: &[(&str, &str)]) -> Result<Result<Tokens, OAuthError>> {
        let mut form = vec![
            ("client_id", self.client_id.as_str()),
            ("scope", self.scope.as_str()),
        ];
        form.extend_from_slice(params);
        let response = self
            .token_http
            .post(&self.token_endpoint)
            .form(&form)
            .send()
            .await
            .context("could not reach the login server")?;
        // Only the grant type, since the other parameters are secrets.
        let grant = params.iter().find(|(name, _)| *name == "grant_type");
        tracing::debug!(
            grant = grant.map(|(_, value)| *value),
            status = %response.status(),
            "the login server answered a token request"
        );
        if response.status().is_success() {
            Ok(Ok(response
                .json()
                .await
                .context("unexpected token response")?))
        } else {
            Ok(Err(response
                .json()
                .await
                .context("unexpected error from the login server")?))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;
    use crate::testing::fake_model;

    /// Keeps the links it is asked to open in the browser.
    #[derive(Default)]
    struct Browser(RefCell<Vec<String>>);

    impl Prompts for Browser {
        fn notice(&self, _: &str) {}

        fn confirm_code(&self, link: &str, _: &str) {
            self.0.borrow_mut().push(link.to_string());
        }

        fn username_and_password(&self) -> Result<(String, String)> {
            bail!("not asked in this test")
        }
    }

    #[tokio::test]
    async fn logs_in_over_plain_http_only_to_this_machine() {
        let elsewhere = "http://login.example.org/realms/damap";
        let here = "http://localhost:9/realms/damap";
        // What a login server on this machine says, each in turn.
        let (issuer, server) = fake_model(vec![
            json!({ "token_endpoint": format!("{elsewhere}/token") }),
            json!({
                "token_endpoint": format!("{here}/token"),
                "device_authorization_endpoint": format!("{elsewhere}/device"),
            }),
            json!({
                "token_endpoint": format!("{here}/token"),
                "device_authorization_endpoint": "https://login.example.org/device",
            }),
        ]);
        let mut refused = Vec::new();
        for issuer in [elsewhere, &issuer, &issuer, &issuer] {
            let discovered = Oidc::discover(issuer, "damap", "openid").await;
            refused.push(discovered.err().map(|e| format!("{e:#}")));
        }

        assert_eq!(
            refused,
            [
                Some(unencrypted(elsewhere)),
                Some(unencrypted(&format!("{elsewhere}/token"))),
                Some(unencrypted(&format!("{elsewhere}/device"))),
                None,
            ]
        );
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn opens_a_login_page_over_plain_http_only_on_this_machine() {
        let page = "http://login.example.org/device?user_code=ABCD-EFGH";
        let (device_endpoint, device) = fake_model(vec![json!({
            "device_code": "secret",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://login.example.org/device",
            "verification_uri_complete": page,
            "expires_in": 60,
        })]);
        let (issuer, discovery) = fake_model(vec![json!({
            "token_endpoint": "http://localhost:9/token",
            "device_authorization_endpoint": device_endpoint,
        })]);
        let oidc = Oidc::discover(&issuer, "damap", "openid").await.unwrap();
        let browser = Browser::default();

        let refused = oidc.login(&browser).await.err().map(|e| format!("{e:#}"));

        assert_eq!(refused, Some(unencrypted(page)));
        assert!(browser.0.borrow().is_empty());
        assert_eq!(discovery.join().unwrap().len(), 1);
        assert_eq!(device.join().unwrap().len(), 1);
    }
}
