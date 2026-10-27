use std::time::Duration;

use anyhow::{Context, Result, bail};
use genai::adapter::AdapterKind;
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, Headers, ModelIden, ServiceTarget, WebConfig, webc};
use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use reqwest::Client as HttpClient;
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use serde::Deserialize;
use serde_json::Value;

use crate::net;

/// How long listing the models may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(15);

pub async fn list_models(url: &str, api_key: Option<&str>) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Models {
        data: Vec<Model>,
    }
    #[derive(Deserialize)]
    struct Model {
        id: String,
    }

    // As `web_url` reads it, so that addresses `same_url` takes for the same API get the same
    // requests.
    let url = &net::web_url(url)?;
    // Unlike DAMAP's client, it follows any redirect: an API that takes no key may be on another
    // machine over plain HTTP, and reqwest drops the key on a redirect to another host or scheme.
    let http = net::for_server(
        HttpClient::builder()
            .user_agent(concat!("fair-witness/", env!("CARGO_PKG_VERSION")))
            .timeout(LIST_TIMEOUT),
        url,
    )
    .build()?;
    let url = net::under(url, "models")?;
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

pub fn openai_compatible(url: &str, api_key: Option<String>) -> Result<Client> {
    // As `web_url` reads it, so that addresses `same_url` takes for the same API get the same
    // requests.
    let url = &net::web_url(url)?;
    // The client genai builds by default, but for this API.
    let http = net::for_server(
        WebConfig::default().apply_to_builder(HttpClient::builder()),
        url,
    )
    .build()
    .with_context(|| format!("could not set up a client for the AI API at {url}"))?;
    // genai joins its paths to the endpoint and keeps its query.
    let endpoint = Endpoint::from_owned(net::under(url, "")?);
    let auth = match api_key {
        Some(key) => AuthData::from_single(key),
        // genai's OpenAI adapter sends `Authorization: Bearer ` even without a key, which an API
        // that takes none can refuse. The override sends the chat request to the URL genai would
        // use, without that header, the only one the adapter sets.
        None => AuthData::RequestOverride {
            url: net::under(url, "chat/completions")?,
            headers: Headers::default(),
        },
    };
    Client::builder()
        .with_reqwest(http)
        .with_service_target_resolver_fn(move |target: ServiceTarget| {
            Ok(ServiceTarget {
                endpoint: endpoint.clone(),
                auth: auth.clone(),
                model: ModelIden::new(AdapterKind::OpenAI, target.model.model_name),
            })
        })
        .build()
        .with_context(|| format!("could not set up a client for the AI API at {url}"))
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

/// What the API said when it refused a request with a 4xx status that sending it again would
/// not change. OpenAI's SDK sends a request again after 408 Request Timeout, 409 Conflict and 429
/// Too Many Requests, which a busy API answers.
pub fn refused(error: &genai::Error) -> Option<String> {
    let genai::Error::WebModelCall {
        webc_error: webc::Error::ResponseFailedStatus { status, body, .. },
        ..
    } = error
    else {
        return None;
    };
    let busy = [
        StatusCode::REQUEST_TIMEOUT,
        StatusCode::CONFLICT,
        StatusCode::TOO_MANY_REQUESTS,
    ];
    (status.is_client_error() && !busy.contains(status)).then(|| message(body))
}

/// Why a request failed, in one line and without the request and the full answer that genai's
/// errors hold.
pub fn why_failed(error: &genai::Error) -> String {
    let why = match error {
        genai::Error::WebModelCall { webc_error, .. } => match webc_error {
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
        },
        // genai's own text quotes the whole request, with the DMP and every tool result.
        genai::Error::ChatResponseGeneration { cause, .. } => {
            format!(
                "the model's reply could not be read: {}",
                without_values(cause)
            )
        }
        e => e.to_string(),
    };
    why.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `cause` with the values it quotes left out, since the reply that serde could not read may
/// repeat what the model read in files. serde quotes a string in double quotes, escaped as Rust
/// does, and a number, a character or a field name in backticks.
fn without_values(cause: &str) -> String {
    let mut kept = String::with_capacity(cause.len());
    let mut rest = cause;
    while let Some(at) = rest.find(['"', '`']) {
        let (before, quoted) = rest.split_at(at);
        kept.push_str(before);
        let quote = &quoted[..1];
        let Some(len) = quoted_len(quoted) else {
            // Without its end, the rest may all be the value.
            kept.push_str(&format!("{quote}…"));
            return kept;
        };
        kept.push_str(&format!("{quote}…{quote}"));
        rest = &quoted[len..];
    }
    kept.push_str(rest);
    kept
}

/// How long the quoted value at the start of `text` is, with its quotes.
fn quoted_len(text: &str) -> Option<usize> {
    let mut chars = text.char_indices();
    let (_, quote) = chars.next()?;
    while let Some((at, c)) = chars.next() {
        if c == quote {
            return Some(at + c.len_utf8());
        }
        if c == '\\' && quote == '"' {
            chars.next();
        }
    }
    None
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

    use genai::chat::{ChatMessage, ChatRequest};

    use super::*;
    use crate::testing::{EFFORT_NOT_ALLOWED, fake_model, one_token};

    #[tokio::test]
    async fn lists_the_models_a_server_offers() {
        let (url, server) = fake_model(vec![json!({
            "object": "list",
            "data": [
                { "id": "qwen3:8b", "object": "model" },
                { "id": "llama3.2", "object": "model" }
            ]
        })]);

        let models = list_models(&format!("{url}/"), Some("sk-test"))
            .await
            .unwrap();

        assert_eq!(models, ["llama3.2", "qwen3:8b"]);
        let (head, _) = &server.join().unwrap()[0];
        assert!(head.starts_with("GET /v1/models "), "{head}");
        assert!(head.contains("authorization: Bearer sk-test"), "{head}");
    }

    // Windows refuses to connect to 0.0.0.0.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn follows_a_redirect_from_an_api_on_another_machine() {
        use crate::testing::{Reply, redirect};

        // A proxy from the environment would get the requests to 0.0.0.0 instead of the fake API,
        // and a test cannot unset it while other tests run.
        let proxies = ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"];
        if let Some(name) = proxies
            .into_iter()
            .find(|name| std::env::var_os(name).is_some())
        {
            eprintln!("skipped, since {name} is set");
            return;
        }
        let (url, server) = fake_model(vec![
            redirect("/v1/models/"),
            Reply::from(json!({ "object": "list", "data": [{ "id": "qwen3:8b" }] })),
        ]);
        // 0.0.0.0 reaches this machine on Linux and macOS, but fair-witness cannot tell that it
        // is not another one.
        let url = url.replace("127.0.0.1", "0.0.0.0");

        assert_eq!(list_models(&url, None).await.unwrap(), ["qwen3:8b"]);
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sends_an_authorization_header_only_with_a_key() {
        let (url, server) = fake_model(vec![one_token(), one_token()]);
        for api_key in [Some("sk-test".to_string()), None] {
            openai_compatible(&url, api_key)
                .unwrap()
                .exec_chat(
                    "small",
                    ChatRequest::new(vec![ChatMessage::user("Hi")]),
                    None,
                )
                .await
                .unwrap();
        }

        let requests = server.join().unwrap();
        let heads: Vec<(&str, bool)> = requests
            .iter()
            .map(|(head, _)| {
                let line = head.lines().next().unwrap_or_default();
                (line, head.to_lowercase().contains("authorization:"))
            })
            .collect();
        assert_eq!(
            heads,
            [
                ("POST /v1/chat/completions HTTP/1.1", true),
                ("POST /v1/chat/completions HTTP/1.1", false)
            ]
        );
    }

    #[tokio::test]
    async fn sends_the_same_requests_to_addresses_read_alike() {
        let models = || json!({ "object": "list", "data": [] });
        let (url, server) = fake_model(vec![
            models(),
            one_token(),
            one_token(),
            models(),
            one_token(),
            one_token(),
        ]);
        let alike = [
            [
                format!("{url}//."),
                format!("{}/", url.replace("http", "HTTP")),
            ],
            [format!("{url}?tenant=team"), format!("{url}/?tenant=team")],
        ];

        for [one, other] in &alike {
            assert!(net::same_url(one, other), "{one} and {other}");
            list_models(one, None).await.unwrap();
            for (url, api_key) in [(one, Some("sk-test".to_string())), (other, None)] {
                openai_compatible(url, api_key)
                    .unwrap()
                    .exec_chat(
                        "small",
                        ChatRequest::new(vec![ChatMessage::user("Hi")]),
                        None,
                    )
                    .await
                    .unwrap();
            }
        }

        let requests = server.join().unwrap();
        let lines: Vec<&str> = requests
            .iter()
            .map(|(head, _)| head.lines().next().unwrap_or_default())
            .collect();
        assert_eq!(
            lines,
            [
                "GET /v1/models HTTP/1.1",
                "POST /v1/chat/completions HTTP/1.1",
                "POST /v1/chat/completions HTTP/1.1",
                "GET /v1/models?tenant=team HTTP/1.1",
                "POST /v1/chat/completions?tenant=team HTTP/1.1",
                "POST /v1/chat/completions?tenant=team HTTP/1.1"
            ]
        );
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
