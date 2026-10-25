use std::time::Duration;

use anyhow::{Context, Result, bail};
use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ModelIden, ServiceTarget, webc};
use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use reqwest::Client as HttpClient;
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use serde::Deserialize;

pub async fn list_models(
    http: &HttpClient,
    url: &str,
    api_key: Option<&str>,
) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Models {
        data: Vec<Model>,
    }
    #[derive(Deserialize)]
    struct Model {
        id: String,
    }

    let url = format!("{}models", base_url(url));
    let mut request = http.get(&url);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("could not reach {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("{url} answered {status}; is the URL right, and the API key?");
    }
    let models: Models = response
        .json()
        .await
        .with_context(|| format!("{url} does not look like an OpenAI-compatible API"))?;
    let mut ids: Vec<String> = models.data.into_iter().map(|model| model.id).collect();
    ids.sort();
    Ok(ids)
}

fn base_url(url: &str) -> String {
    format!("{}/", url.trim_end_matches('/'))
}

pub fn openai_compatible(url: &str, api_key: Option<String>) -> Client {
    let endpoint = Endpoint::from_owned(base_url(url));
    let auth = AuthData::from_single(api_key.unwrap_or_default());
    Client::builder()
        .with_service_target_resolver_fn(move |target: ServiceTarget| {
            Ok(ServiceTarget {
                endpoint: endpoint.clone(),
                auth: auth.clone(),
                model: ModelIden::new(AdapterKind::OpenAI, target.model.model_name),
            })
        })
        .build()
}

/// A request the API refused with 429 Too Many Requests.
pub struct TooManyRequests {
    /// How long the API asks to wait, if it says.
    pub retry_after: Option<Duration>,
    /// What the API answered.
    pub body: String,
}

pub fn too_many_requests(error: &genai::Error) -> Option<TooManyRequests> {
    let genai::Error::WebModelCall {
        webc_error:
            webc::Error::ResponseFailedStatus {
                status,
                body,
                headers,
            },
        ..
    } = error
    else {
        return None;
    };
    if *status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    let header = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok());
    Some(TooManyRequests {
        retry_after: retry_after(header, body),
        body: body.clone(),
    })
}

/// The forms of a date in HTTP, always in GMT: the one in use, then two obsolete ones that
/// recipients must still accept.
const HTTP_DATES: [&str; 3] = [
    "%a, %d %b %Y %H:%M:%S GMT",
    "%A, %d-%b-%y %H:%M:%S GMT",
    "%a %b %e %H:%M:%S %Y",
];

/// Reads the wait from the `Retry-After` header, in seconds or as a date, or else from a body
/// such as Aqueduct's, whose message ends in "(Retry after 4s)".
fn retry_after(header: Option<&str>, body: &str) -> Option<Duration> {
    let from_header = header.and_then(|value| {
        let value = value.trim();
        if let Ok(seconds) = value.parse() {
            return wait(seconds);
        }
        let date = HTTP_DATES
            .iter()
            .find_map(|format| DateTime::strptime(format, value).ok())?
            .to_zoned(TimeZone::UTC)
            .ok()?
            .timestamp();
        // A date that has passed asks for no wait at all.
        Some(Duration::try_from(date.duration_since(Timestamp::now())).unwrap_or_default())
    });
    from_header.or_else(|| {
        let body = body.to_ascii_lowercase();
        let (_, rest) = body.split_once("retry after ")?;
        let number = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let (seconds, unit) = rest.split_at(number);
        if !unit.starts_with('s') {
            return None;
        }
        wait(seconds.parse().ok()?)
    })
}

/// A wait of `seconds`, or none for a negative one. One too long for a `Duration` stays too
/// long to wait for.
fn wait(seconds: f64) -> Option<Duration> {
    (seconds >= 0.0).then(|| Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testing::fake_model;

    #[tokio::test]
    async fn lists_the_models_a_server_offers() {
        let (url, server) = fake_model(vec![json!({
            "object": "list",
            "data": [
                { "id": "qwen3:8b", "object": "model" },
                { "id": "llama3.2", "object": "model" }
            ]
        })]);

        let models = list_models(&HttpClient::new(), &format!("{url}/"), Some("sk-test"))
            .await
            .unwrap();

        assert_eq!(models, ["llama3.2", "qwen3:8b"]);
        let (head, _) = &server.join().unwrap()[0];
        assert!(head.starts_with("GET /v1/models "), "{head}");
        assert!(head.contains("authorization: Bearer sk-test"), "{head}");
    }

    #[test]
    fn reads_the_wait_from_the_header_or_else_the_body() {
        let aqueduct = r#"{"error": {"message": "Rate limit exceeded. Request limit (12.5/min) (Retry after 44s).", "type": "rate_limit_error"}}"#;
        let secs = |s| Some(Duration::from_secs(s));
        assert_eq!(retry_after(Some("3"), aqueduct), secs(3));
        assert_eq!(retry_after(Some("soon"), aqueduct), secs(44));
        assert_eq!(retry_after(None, aqueduct), secs(44));
        assert_eq!(retry_after(None, "Retry after 2 minutes"), None);
        assert_eq!(retry_after(None, "Too many requests"), None);
        assert_eq!(
            retry_after(Some("18446744073709551616"), ""),
            Some(Duration::MAX)
        );
        assert_eq!(
            retry_after(None, "Retry after 99999999999999999999s"),
            Some(Duration::MAX)
        );
        assert_eq!(retry_after(Some("-1"), aqueduct), secs(44));
        for date in [
            "Thu, 01 Jan 1970 00:00:01 GMT",
            "Thursday, 01-Jan-70 00:00:01 GMT",
            "Thu Jan  1 00:00:01 1970",
        ] {
            assert_eq!(retry_after(Some(date), ""), secs(0), "{date}");
        }
        let in_a_minute = (Timestamp::now() + Duration::from_secs(60)).to_zoned(TimeZone::UTC);
        for format in HTTP_DATES {
            let date = in_a_minute.strftime(format).to_string();
            let wait = retry_after(Some(&date), "").unwrap();
            assert!(
                wait > secs(58).unwrap() && wait <= secs(60).unwrap(),
                "{date}: {wait:?}"
            );
        }
    }
}
