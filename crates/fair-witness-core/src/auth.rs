use std::fmt;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;

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

/// The fields of a request to the login server that hold a secret, which an error it answers
/// with must not repeat.
const SECRETS: [&str; 4] = ["password", "refresh_token", "device_code", "token"];

#[derive(Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct Discovery {
    token_endpoint: String,
    device_authorization_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
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

impl OAuthError {
    /// This error without the secrets in `form`, in case the login server repeats one.
    fn hiding_secrets_in(mut self, form: &[(&str, &str)]) -> Self {
        let secrets = form
            .iter()
            .filter(|(name, value)| SECRETS.contains(name) && !value.is_empty());
        for (_, secret) in secrets {
            self.error = self.error.replace(secret, "[hidden]");
            self.error_description = self
                .error_description
                .map(|description| description.replace(secret, "[hidden]"));
        }
        self
    }
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.error_description {
            Some(description) => write!(f, "{description} ({})", self.error),
            None => write!(f, "{}", self.error),
        }
    }
}

/// An error status from an endpoint of the login server, with the OAuth error in its body if it
/// held one.
struct Refusal {
    endpoint: String,
    status: StatusCode,
    error: Option<OAuthError>,
}

impl Refusal {
    /// Whether the login server refused with the OAuth error `code`.
    fn is(&self, code: &str) -> bool {
        self.error.as_ref().is_some_and(|error| error.error == code)
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} answered {}", self.endpoint, self.status)?;
        match &self.error {
            Some(error) => write!(f, ": {error}"),
            None => Ok(()),
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

/// Posts `form` to `endpoint` of the login server, and fails when it could not be reached.
async fn post(
    http: &Client,
    endpoint: &str,
    form: &[(&str, &str)],
) -> Result<Result<Response, Refusal>> {
    let response = http
        .post(endpoint)
        .form(form)
        .send()
        .await
        .context("could not reach the login server")?;
    let status = response.status();
    if status.is_success() {
        return Ok(Ok(response));
    }
    // A proxy in front of the login server answers with a page of its own, such as for 502 Bad
    // Gateway, which holds no OAuth error.
    let error = response
        .json::<OAuthError>()
        .await
        .ok()
        .map(|error| error.hiding_secrets_in(form));
    Ok(Err(Refusal {
        endpoint: endpoint.to_string(),
        status,
        error,
    }))
}

/// Reads what `endpoint` answered in `response` as `T`.
async fn read<T: DeserializeOwned>(endpoint: &str, response: Response) -> Result<T> {
    let status = response.status();
    response
        .json()
        .await
        // The message names the endpoint already.
        .map_err(reqwest::Error::without_url)
        .with_context(|| format!("{endpoint} answered {status} with something unexpected"))
}

pub struct Oidc {
    /// For the token endpoint, which may be on another machine than the issuer and so need
    /// another proxy setting.
    token_http: Client,
    client_id: String,
    scope: String,
    token_endpoint: String,
    device_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
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
            revocation_endpoint: discovery.revocation_endpoint,
        })
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens> {
        self.token_request(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .await?
        .map_err(|e| anyhow!("session could not be refreshed: {e}"))
    }

    /// Ends the session `refresh_token` belongs to at the login server, as RFC 7009 describes.
    pub async fn revoke(&self, refresh_token: &str) -> Result<()> {
        let Some(endpoint) = &self.revocation_endpoint else {
            bail!("the login server names no endpoint to end a session at");
        };
        // Checked here rather than in `discover`, so that it stops only logging out.
        if in_the_clear(endpoint) {
            bail!(unencrypted(endpoint));
        }
        let form = [
            ("client_id", self.client_id.as_str()),
            ("token", refresh_token),
            ("token_type_hint", "refresh_token"),
        ];
        // The login server answers 200 also for a token that has already expired, so any success
        // means the session is over.
        match post(&net::client(endpoint, TIMEOUT)?, endpoint, &form).await? {
            Ok(_) => Ok(()),
            Err(e) => bail!("{e}"),
        }
    }

    pub async fn login(&self, prompts: &dyn Prompts) -> Result<Tokens> {
        if let Some(endpoint) = &self.device_endpoint {
            match self.start_device_flow(endpoint).await? {
                Ok(authorization) => {
                    return self.finish_device_flow(authorization, prompts).await;
                }
                Err(e) if e.is("unauthorized_client") => {
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
    ) -> Result<Result<DeviceAuthorization, Refusal>> {
        let form = [
            ("client_id", self.client_id.as_str()),
            ("scope", self.scope.as_str()),
        ];
        match post(&net::client(endpoint, TIMEOUT)?, endpoint, &form).await? {
            Ok(response) => Ok(Ok(read(endpoint, response).await?)),
            Err(refusal) => Ok(Err(refusal)),
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
        let mut wait = interval;
        let mut failure = None;
        let params = [
            ("grant_type", DEVICE_CODE_GRANT),
            ("device_code", authorization.device_code.as_str()),
        ];
        // A poll after the code expired would only be told so, and hide why the last one failed.
        while Instant::now() + wait < deadline {
            tokio::time::sleep(wait).await;
            // The person may have confirmed the code already, so a request that fails on the way
            // or meets a server error must not end the login while the code is valid.
            failure = match self.token_request(&params).await {
                Ok(Ok(tokens)) => return Ok(tokens),
                Ok(Err(e)) if e.is("authorization_pending") => None,
                Ok(Err(e)) if e.is("slow_down") => {
                    interval += Duration::from_secs(5);
                    None
                }
                Ok(Err(e)) if e.status.is_server_error() => Some(anyhow!("{e}")),
                Ok(Err(e)) => bail!("login failed: {e}"),
                Err(e) => Some(e),
            };
            wait = match &failure {
                Some(e) => {
                    prompts.notice(&format!("Checking the login failed, trying again: {e:#}"));
                    // RFC 8628 asks to poll less often after a failed request, and the second
                    // keeps a server that asks for no interval from getting a flood of them.
                    (wait * 2).max(Duration::from_secs(1))
                }
                None => interval,
            };
        }
        let timed_out = "login timed out; please try again";
        Err(match failure {
            Some(e) => e.context(timed_out),
            None => anyhow!(timed_out),
        })
    }

    async fn password_login(&self, prompts: &dyn Prompts) -> Result<Tokens> {
        let (username, password) = prompts.username_and_password()?;
        self.token_request(&[
            ("grant_type", "password"),
            ("username", &username),
            ("password", &password),
        ])
        .await?
        .map_err(|e| anyhow!("login failed: {e}"))
    }

    async fn token_request(&self, params: &[(&str, &str)]) -> Result<Result<Tokens, Refusal>> {
        let mut form = vec![
            ("client_id", self.client_id.as_str()),
            ("scope", self.scope.as_str()),
        ];
        form.extend_from_slice(params);
        let answer = post(&self.token_http, &self.token_endpoint, &form).await?;
        // Only the grant type, since the other parameters are secrets.
        let grant = params.iter().find(|(name, _)| *name == "grant_type");
        let status = match &answer {
            Ok(response) => response.status(),
            Err(refusal) => refusal.status,
        };
        tracing::debug!(
            grant = grant.map(|(_, value)| *value),
            %status,
            "the login server answered a token request"
        );
        match answer {
            Ok(response) => Ok(Ok(read(&self.token_endpoint, response).await?)),
            Err(refusal) => Ok(Err(refusal)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::{Value, json};

    use super::*;
    use crate::testing::{answer, fake_model, hang_up, page};

    /// Keeps the links it is asked to open in the browser, and the notices.
    #[derive(Default)]
    struct Browser(RefCell<Vec<String>>, RefCell<Vec<String>>);

    impl Prompts for Browser {
        fn notice(&self, message: &str) {
            self.1.borrow_mut().push(message.to_string());
        }

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

    const BAD_GATEWAY: &str = "<html><body><h1>502 Bad Gateway</h1></body></html>";

    /// The login server at `url`, with all its endpoints there.
    fn login_server(url: &str) -> Oidc {
        Oidc {
            token_http: net::client(url, TIMEOUT).unwrap(),
            client_id: "damap".to_string(),
            scope: "openid".to_string(),
            token_endpoint: format!("{url}/token"),
            device_endpoint: Some(format!("{url}/device")),
            revocation_endpoint: Some(format!("{url}/revoke")),
        }
    }

    #[tokio::test]
    async fn names_the_endpoint_and_status_of_an_error_page() {
        let (url, server) = fake_model(vec![
            page("502 Bad Gateway", BAD_GATEWAY),
            page("502 Bad Gateway", BAD_GATEWAY),
            page("200 OK", BAD_GATEWAY),
        ]);
        let oidc = login_server(&url);

        let failed = [
            oidc.login(&Browser::default()).await.err(),
            oidc.refresh("secret").await.err(),
            oidc.refresh("secret").await.err(),
        ];

        assert_eq!(
            failed.map(|e| e.map(|e| format!("{e:#}"))),
            [
                Some(format!(
                    "login failed: {url}/device answered 502 Bad Gateway"
                )),
                Some(format!(
                    "session could not be refreshed: {url}/token answered 502 Bad Gateway"
                )),
                Some(format!(
                    "{url}/token answered 200 OK with something unexpected: error decoding \
                     response body: expected value at line 1 column 1"
                )),
            ]
        );
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn keeps_polling_through_failed_requests() {
        let (url, server) = fake_model(vec![
            answer(
                "200 OK",
                json!({
                    "device_code": "secret",
                    "user_code": "ABCD-EFGH",
                    "verification_uri": "https://login.example.org/device",
                    "expires_in": 60,
                    "interval": 0,
                }),
            ),
            hang_up(),
            answer(
                "400 Bad Request",
                json!({ "error": "authorization_pending" }),
            ),
            page("502 Bad Gateway", BAD_GATEWAY),
            answer(
                "200 OK",
                json!({ "access_token": "access", "refresh_token": "refresh" }),
            ),
        ]);
        let browser = Browser::default();

        let tokens = login_server(&url).login(&browser).await.unwrap();

        assert_eq!(
            (
                tokens.access_token.as_str(),
                tokens.refresh_token.as_deref()
            ),
            ("access", Some("refresh"))
        );
        assert_eq!(*browser.0.borrow(), ["https://login.example.org/device"]);
        assert_eq!(
            *browser.1.borrow(),
            [
                format!(
                    "Checking the login failed, trying again: could not reach the login server: \
                     error sending request for url ({url}/token): client error (SendRequest): \
                     connection closed before message completed"
                ),
                format!(
                    "Checking the login failed, trying again: {url}/token answered 502 Bad Gateway"
                ),
            ]
        );
        for (head, form) in server.join().unwrap().split_off(1) {
            assert!(head.starts_with("POST /v1/token "), "{head}");
            assert!(
                form.as_str().unwrap().contains("device_code=secret"),
                "{form}"
            );
        }
    }

    #[tokio::test]
    async fn reports_the_last_failure_when_the_code_expires() {
        let (url, server) = fake_model(vec![
            answer(
                "200 OK",
                json!({
                    "device_code": "secret",
                    "user_code": "ABCD-EFGH",
                    "verification_uri": "https://login.example.org/device",
                    "expires_in": 1,
                    "interval": 0,
                }),
            ),
            page("502 Bad Gateway", BAD_GATEWAY),
        ]);

        let failed = login_server(&url).login(&Browser::default()).await;

        assert_eq!(
            failed.err().map(|e| format!("{e:#}")),
            Some(format!(
                "login timed out; please try again: {url}/token answered 502 Bad Gateway"
            ))
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn hides_the_tokens_a_login_server_repeats() {
        let repeating = |token: &str| {
            answer(
                "400 Bad Request",
                json!({
                    "error": format!("invalid_grant {token}"),
                    "error_description": format!("Token {token} is not active"),
                }),
            )
        };
        let (url, _) = fake_model(vec![
            repeating("refresh-secret"),
            repeating("revoked-secret"),
        ]);
        let oidc = login_server(&url);

        let failed = [
            oidc.refresh("refresh-secret").await.err(),
            oidc.revoke("revoked-secret").await.err(),
        ];

        assert_eq!(
            failed.map(|e| e.map(|e| format!("{e:#}"))),
            [
                Some(format!(
                    "session could not be refreshed: {url}/token answered 400 Bad Request: Token \
                     [hidden] is not active (invalid_grant [hidden])"
                )),
                Some(format!(
                    "{url}/revoke answered 400 Bad Request: Token [hidden] is not active \
                     (invalid_grant [hidden])"
                )),
            ]
        );
    }

    #[tokio::test]
    async fn ends_the_session_at_the_revocation_endpoint() {
        let (url, server) = fake_model(vec![
            answer("200 OK", Value::Null),
            answer(
                "400 Bad Request",
                json!({
                    "error": "unsupported_token_type",
                    "error_description": "Unsupported token type",
                }),
            ),
        ]);
        let (issuer, _) = fake_model(vec![json!({
            "token_endpoint": format!("{url}/token"),
            "revocation_endpoint": format!("{url}/revoke"),
        })]);
        let mut oidc = Oidc::discover(&issuer, "damap", "openid").await.unwrap();

        let ended = oidc.revoke("secret").await;
        let refused = oidc.revoke("secret").await;
        oidc.revocation_endpoint = None;
        let unknown = oidc.revoke("secret").await;

        assert!(ended.is_ok(), "{ended:?}");
        assert_eq!(
            [refused, unknown].map(|result| result.err().map(|e| format!("{e:#}"))),
            [
                Some(format!(
                    "{url}/revoke answered 400 Bad Request: Unsupported token type \
                     (unsupported_token_type)"
                )),
                Some("the login server names no endpoint to end a session at".to_string()),
            ]
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].0.starts_with("POST /v1/revoke "),
            "{}",
            requests[0].0
        );
        assert_eq!(
            requests[0].1,
            json!("client_id=damap&token=secret&token_type_hint=refresh_token")
        );
    }
}
