//! Which servers fair-witness sends secrets to over plain HTTP, and how it reaches them.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::redirect::Policy;
use reqwest::{Client, ClientBuilder};
use url::{Host, Url};

/// Whether a secret sent to `url` would cross the network unencrypted: plain HTTP to another
/// machine than this one. fair-witness reads an address without a scheme as HTTPS.
pub fn in_the_clear(url: &str) -> bool {
    Url::parse(url.trim()).is_ok_and(|url| url.scheme() == "http" && !on_this_machine(&url))
}

fn on_this_machine(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain == "localhost",
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// `builder` for the server at `url`. A server on this machine is reached without a proxy from
/// `HTTP_PROXY` and the like, which could not reach it there and, over plain HTTP, would get
/// what is sent unencrypted.
pub(crate) fn for_server(builder: ClientBuilder, url: &str) -> ClientBuilder {
    if Url::parse(url.trim()).is_ok_and(|url| on_this_machine(&url)) {
        builder.no_proxy()
    } else {
        builder
    }
}

/// A client for the server at `url` whose requests may take `timeout` each.
pub(crate) fn client(url: &str, timeout: Duration) -> Result<Client> {
    for_server(
        Client::builder()
            .user_agent(concat!("fair-witness/", env!("CARGO_PKG_VERSION")))
            .timeout(timeout)
            .redirect(redirects()),
        url,
    )
    .build()
    .with_context(|| format!("could not set up a client for {url}"))
}

/// Follows a redirect to plain HTTP only from this machine to this machine. A 307 or 308 sends
/// the form along, with a password or token in it, and the proxy setting of the client was chosen
/// for where the request started.
fn redirects() -> Policy {
    Policy::custom(|attempt| {
        let from_here = attempt.previous().first().is_some_and(on_this_machine);
        let to = attempt.url();
        if to.scheme() == "http" && !(from_here && on_this_machine(to)) {
            let refusal = format!("refused to follow a redirect to {to} over plain HTTP");
            attempt.error(refusal)
        } else {
            Policy::default().redirect(attempt)
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testing::{Reply, fake_model, redirect};

    #[test]
    fn sends_secrets_over_plain_http_only_to_this_machine() {
        assert_eq!(
            [
                "https://models.example.org/v1",
                "models.example.org/v1",
                "http://127.0.0.1:8080/v1",
                "http://LOCALHOST:1234",
                "http://[::1]:9/v1",
                "http://models.example.org/v1",
                "http://10.0.0.5:8080/v1",
                "HTTP://Models.Example.org",
                "http://127.0.0.1@models.example.org/v1",
                "http://models.example.org\\@127.0.0.1/v1",
                "http://localhost.example.org",
            ]
            .map(in_the_clear),
            [
                false, false, false, false, false, true, true, true, true, true, true
            ]
        );
    }

    #[tokio::test]
    async fn follows_a_redirect_to_plain_http_only_on_this_machine() {
        let (url, server) = fake_model(vec![
            redirect("http://login.example.org/token"),
            redirect("/v1/token"),
            Reply::from(json!({})),
        ]);
        let http = client(&url, Duration::from_secs(5)).unwrap();
        let login = json!({ "password": "user" });

        let refused = http.post(&url).json(&login).send().await.unwrap_err();
        let followed = http.post(&url).json(&login).send().await.unwrap();

        assert_eq!(
            format!("{:#}", anyhow::Error::new(refused)),
            format!(
                "error following redirect for url ({url}): refused to follow a redirect to \
                 http://login.example.org/token over plain HTTP"
            )
        );
        assert!(followed.status().is_success());
        let requests = server.join().unwrap();
        assert!(
            requests[2].0.starts_with("POST /v1/token "),
            "{}",
            requests[2].0
        );
        assert_eq!(requests[2].1, login);
    }
}
