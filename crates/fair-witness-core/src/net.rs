//! Which servers fair-witness sends secrets to over plain HTTP, and how it reaches them.

use reqwest::ClientBuilder;
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
