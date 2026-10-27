use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::{Client, RequestBuilder, Response};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::net::{self, in_the_clear};

/// How long a request to DAMAP may take.
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceConfig {
    pub issuer: String,
    #[serde(rename = "clientID")]
    pub client_id: String,
    pub scope: String,
    pub app_title: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DmpListItem {
    pub id: i64,
    pub title: Option<String>,
    pub project: Option<Project>,
    pub modified: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Project {
    pub title: Option<String>,
}

impl DmpListItem {
    pub fn name(&self) -> Option<&str> {
        let project = self.project.as_ref().and_then(|p| p.title.as_deref());
        [project, self.title.as_deref()]
            .into_iter()
            .flatten()
            .find(|title| !title.trim().is_empty())
    }
}

pub struct Damap {
    http: Client,
    url: String,
    access_token: Option<String>,
}

impl Damap {
    /// DAMAP at `url`, unless it uses plain HTTP to another machine, where anyone along the way
    /// could read the login. `url` is read as `web_url` reads it, so that addresses `same_url`
    /// takes for the same DAMAP get the same requests.
    pub fn new(url: &str) -> Result<Self> {
        let url = net::web_url(url)?;
        if in_the_clear(&url) {
            bail!(
                "{url} uses plain HTTP to another machine, which would send the DAMAP login \
                 unencrypted\nUse DAMAP's https:// address, or a DAMAP on this machine, such as \
                 http://localhost:8085"
            );
        }
        Ok(Self {
            http: net::client(&url, TIMEOUT)?,
            url,
            access_token: None,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn set_access_token(&mut self, token: String) {
        self.access_token = Some(token);
    }

    pub async fn instance_config(&self) -> Result<InstanceConfig> {
        let response = send(
            self.http.get(format!("{}/api/config", self.url)),
            "/api/config",
        )
        .await
        .with_context(|| format!("could not reach {}", self.url))?;
        if !response.status().is_success() {
            bail!(
                "{} answered {} on /api/config; is this DAMAP?",
                self.url,
                response.status()
            );
        }
        response
            .json()
            .await
            .with_context(|| format!("{} does not look like DAMAP", self.url))
    }

    pub async fn list_dmps(&self) -> Result<Vec<DmpListItem>> {
        self.get_json("/api/dmps/list").await
    }

    pub async fn madmp(&self, id: i64) -> Result<serde_json::Value> {
        self.get_json(&format!("/api/madmp/{id}")).await
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let request = self.http.get(format!("{}{path}", self.url));
        send_json(self.authorize(request), path).await
    }

    fn authorize(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.access_token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }
}

async fn send(request: RequestBuilder, path: &str) -> reqwest::Result<Response> {
    let sent = Instant::now();
    let response = request.send().await;
    let elapsed_ms = sent.elapsed().as_millis();
    match &response {
        Ok(response) => {
            tracing::debug!(path, status = %response.status(), elapsed_ms, "DAMAP answered")
        }
        Err(e) => tracing::warn!(path, elapsed_ms, "DAMAP could not be reached: {e}"),
    }
    response
}

async fn send_json<T: DeserializeOwned>(request: RequestBuilder, path: &str) -> Result<T> {
    let response = send(request, path)
        .await
        .with_context(|| format!("request to {path} failed"))?;
    match response.status() {
        status if status.is_success() => response
            .json()
            .await
            .with_context(|| format!("unexpected response from {path}")),
        StatusCode::UNAUTHORIZED => bail!("DAMAP rejected the login token on {path}"),
        StatusCode::FORBIDDEN => bail!("not allowed to access {path}"),
        status => bail!("{path} failed with {status}"),
    }
}
