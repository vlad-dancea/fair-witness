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
use serde_json::Value;

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

/// What the API answered when it refused a request with 400 Bad Request and asked to name
/// `reasoning_effort` in `allowed_openai_params`, as LiteLLM, which Aqueduct is built on, does
/// for a model it does not know to take an effort.
pub fn asks_to_allow_effort(error: &genai::Error) -> Option<String> {
    let genai::Error::WebModelCall {
        webc_error: webc::Error::ResponseFailedStatus { status, body, .. },
        ..
    } = error
    else {
        return None;
    };
    (*status == StatusCode::BAD_REQUEST && asks_for_effort(body)).then(|| body.clone())
}

/// What the API said when it refused a request with a 4xx status other than 429 Too Many
/// Requests, which sending it again would not change.
pub fn refused(error: &genai::Error) -> Option<String> {
    let genai::Error::WebModelCall {
        webc_error: webc::Error::ResponseFailedStatus { status, body, .. },
        ..
    } = error
    else {
        return None;
    };
    (status.is_client_error() && *status != StatusCode::TOO_MANY_REQUESTS).then(|| message(body))
}

/// Why a request failed, without the request and the full answer that genai's errors hold.
pub fn why_failed(error: &genai::Error) -> String {
    let genai::Error::WebModelCall { webc_error, .. } = error else {
        return error.to_string().lines().collect::<Vec<_>>().join(" ");
    };
    match webc_error {
        webc::Error::ResponseFailedStatus { status, body, .. } => match message(body) {
            message if message.is_empty() => format!("the API answered {status}"),
            message => format!("the API answered {status}: {message}"),
        },
        webc::Error::ResponseFailedNotJson { content_type, .. } => {
            format!("the API answered with {content_type} instead of JSON")
        }
        webc::Error::ResponseFailedInvalidJson { cause, .. } => {
            format!("the API answered with invalid JSON: {cause}")
        }
        webc::Error::Reqwest(e) => {
            let mut why = format!("could not reach the API: {e}");
            let mut source = std::error::Error::source(e);
            while let Some(cause) = source {
                why.push_str(&format!(": {cause}"));
                source = cause.source();
            }
            why
        }
        e => e.to_string(),
    }
}

/// The message in an error body, which OpenAI and the APIs like it give as
/// `{"error": {"message": "..."}}` and others in a field of its own, or else the whole body.
fn message(body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let message = parsed.as_ref().and_then(|body| {
        let error = body.get("error");
        [
            error.and_then(|error| error.get("message")),
            error,
            body.get("message"),
            body.get("detail"),
        ]
        .into_iter()
        .flatten()
        .find_map(Value::as_str)
    });
    message.unwrap_or(body).trim().to_string()
}

/// Whether `body` asks to send `allowed_openai_params` with `reasoning_effort` alone, the one
/// parameter the request goes again with. A request refused for others too, such as
/// `tool_choice`, would only be refused again. Gateways rename LiteLLM in their messages, as
/// Aqueduct's `aqueduct.UnsupportedParamsError` does, so only what the message asks for counts.
fn asks_for_effort(body: &str) -> bool {
    body.split_once("allowed_openai_params=[")
        .and_then(|(_, asked)| asked.split_once(']'))
        .is_some_and(|(asked, _)| {
            asked
                .split(',')
                .map(|name| name.trim().trim_matches('\''))
                .eq(["reasoning_effort"])
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
        // A date that seems to have passed, such as when the clocks differ, gives no hint, so
        // that the waits grow as without one instead of the retries going out at once.
        Duration::try_from(http_date(value)?.duration_since(Timestamp::now())).ok()
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

fn http_date(value: &str) -> Option<Timestamp> {
    let date = HTTP_DATES
        .iter()
        .find_map(|format| DateTime::strptime(format, value).ok())?;
    Some(date.to_zoned(TimeZone::UTC).ok()?.timestamp())
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
    use crate::testing::{EFFORT_NOT_ALLOWED, fake_model};

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
        assert_eq!(retry_after(Some("Sun, 06 Nov 1994 08:49:37 GMT"), ""), None);
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

    #[test]
    fn sees_when_the_api_asks_to_allow_the_effort() {
        let body = |message: &str| {
            json!({ "error": { "message": message, "type": "invalid_request_error" } }).to_string()
        };
        let litellm = "litellm.UnsupportedParamsError: openai does not support parameters: \
                       ['tool_choice', 'reasoning_effort'], for model=qwen3. To drop these, set \
                       `litellm.drop_params=True` or for proxy:\n\n`litellm_settings:\n \
                       drop_params: true`\n. \n If you want to use these params dynamically \
                       send allowed_openai_params=['tool_choice', 'reasoning_effort'] in your \
                       request.";
        assert!(asks_for_effort(&body(EFFORT_NOT_ALLOWED)));
        assert!(asks_for_effort(&body(&litellm.replace(
            "'tool_choice', 'reasoning_effort'",
            "'reasoning_effort'"
        ))));
        assert!(!asks_for_effort(&body(litellm)));
        assert!(!asks_for_effort(&body(
            &litellm.replace("'tool_choice', 'reasoning_effort'", "'tool_choice'")
        )));
        assert!(!asks_for_effort(&body(
            "Unrecognized request argument supplied: allowed_openai_params"
        )));
        assert!(!asks_for_effort(&body(
            "openai does not support parameters: ['reasoning_effort']"
        )));
    }

    #[test]
    fn reads_dates_in_each_form_http_allows() {
        let date: Timestamp = "1994-11-06T08:49:37Z".parse().unwrap();
        for value in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
        ] {
            assert_eq!(http_date(value), Some(date), "{value}");
        }
    }
}
