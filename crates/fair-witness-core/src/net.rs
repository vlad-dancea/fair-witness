//! Which servers fair-witness sends secrets to over plain HTTP, and how it reaches them.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::redirect::Policy;
use reqwest::{Client, ClientBuilder};
use url::{Host, Url};

/// Reads a web address as typed, the way fair-witness saves, compares and sends requests to it:
/// `https://` is assumed when no scheme is given, the path loses a trailing `/` and the fragment,
/// which never reaches the server, is dropped. The query stays as typed.
pub fn web_url(input: &str) -> Result<String> {
    let input = input.trim();
    // Only at the start, since a path or query may hold another address.
    let has_scheme = input.split_once("://").is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    let with_scheme = if has_scheme {
        input.to_string()
    } else {
        format!("https://{input}")
    };
    match Url::parse(&with_scheme) {
        Ok(mut url) if matches!(url.scheme(), "http" | "https") && url.has_host() => {
            url.set_fragment(None);
            let path = url.path().trim_end_matches('/').to_string();
            url.set_path(&path);
            Ok(match url.query() {
                Some(_) => url.into(),
                // The url crate writes an empty path as `/`.
                None => url.as_str().trim_end_matches('/').to_string(),
            })
        }
        _ => bail!("{input} is not a web address like https://example.org"),
    }
}

/// Where a request for `path` goes on the server at `url`: under the path of `url`, with its
/// query, as genai's OpenAI adapter builds its requests too.
pub(crate) fn under(url: &str, path: &str) -> Result<String> {
    let mut url = Url::parse(url)
        .with_context(|| format!("{url} is not a web address like https://example.org"))?;
    let joined = format!(
        "{}/{}",
        url.path().trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    url.set_path(&joined);
    Ok(url.into())
}

/// Whether `a` and `b` are the same web address, however each was typed or saved.
pub fn same_url(a: &str, b: &str) -> bool {
    a == b || matches!((web_url(a), web_url(b)), (Ok(a), Ok(b)) if a == b)
}

/// Says that `url` would carry the API key unencrypted, and how to reach the API safely.
pub fn api_key_unencrypted(url: &str) -> String {
    format!(
        "{url} uses plain HTTP to another machine, which would send the API key unencrypted\n\
         Use https://, or reach the API through this machine, such as at http://localhost \
         through an SSH tunnel"
    )
}

/// Whether a secret sent to `url` would cross the network unencrypted: plain HTTP to another
/// machine than this one. An address without a scheme is HTTPS, as [`web_url`] reads it.
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

    #[test]
    fn reads_web_addresses_as_typed() {
        assert_eq!(
            [
                "damap.it.tuwien.ac.at",
                " http://localhost:8085/ ",
                "https://aqueduct.ai.datalab.tuwien.ac.at/v1/",
                "http://[::1]:11434/v1",
                "http://gpu-box:11434/v1",
                "HTTPS://Models.Example.org:443//",
                "https://api.example.org/v1/?api-version=1/",
                "http://localhost:11434/v1/#tag",
                "gpu-box/v1?next=https://example.org",
            ]
            .map(|input| web_url(input).unwrap()),
            [
                "https://damap.it.tuwien.ac.at",
                "http://localhost:8085",
                "https://aqueduct.ai.datalab.tuwien.ac.at/v1",
                "http://[::1]:11434/v1",
                "http://gpu-box:11434/v1",
                "https://models.example.org",
                "https://api.example.org/v1?api-version=1/",
                "http://localhost:11434/v1",
                "https://gpu-box/v1?next=https://example.org",
            ]
        );
    }

    #[test]
    fn rejects_what_is_not_a_web_address() {
        assert_eq!(
            [
                "",
                "ftp://example.org",
                "https://",
                "http://models example.org"
            ]
            .map(|input| web_url(input).unwrap_err().to_string()),
            [
                " is not a web address like https://example.org",
                "ftp://example.org is not a web address like https://example.org",
                "https:// is not a web address like https://example.org",
                "http://models example.org is not a web address like https://example.org",
            ]
        );
    }

    #[test]
    fn compares_web_addresses_however_they_were_typed() {
        assert_eq!(
            [
                ("https://api.example.org/v1", "https://api.example.org/v1/"),
                ("api.example.org/v1", "HTTPS://API.example.org:443/v1"),
                ("not a web address", "not a web address"),
                ("https://api.example.org/v1", "https://api.example.org/v2"),
                ("http://api.example.org/v1", "https://api.example.org/v1"),
                (
                    "https://api.example.org/v1",
                    "https://api.example.org/v1?key=1"
                ),
                (
                    "https://api.example.org/v1?key=1",
                    "https://api.example.org/v1?key=1/"
                ),
                ("not a web address", "not a web address/"),
                // Requests for both go to /v1/chat/completions?key=1.
                (
                    "https://api.example.org/v1/?key=1",
                    "https://api.example.org/v1?key=1"
                ),
                (
                    "http://localhost:11434/v1#tag",
                    "http://localhost:11434/v1/"
                ),
            ]
            .map(|(a, b)| same_url(a, b)),
            [
                true, true, true, false, false, false, false, false, true, true
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
