//! A fake model that answers like an OpenAI-compatible API, for tests here and in the crates
//! that enable the `testing` feature.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::Receiver;
use std::thread;

use serde_json::{Value, json};

/// The head and the body of a request the fake model received, as JSON or else as a string,
/// such as for a form.
pub type Request = (String, Value);

/// A reply in which the model calls the tool `name` with `arguments`.
pub fn tool_call(name: &str, arguments: Value) -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "test",
        "choices": [{
            "index": 0,
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call_{name}"),
                    "type": "function",
                    "function": { "name": name, "arguments": arguments.to_string() }
                }]
            }
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    })
}

/// A reply cut off at the token limit after one token, as the check of a model gets it.
pub fn one_token() -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "test",
        "choices": [{
            "index": 0,
            "finish_reason": "length",
            "message": { "role": "assistant", "content": "Nothing" }
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    })
}

/// What the fake model answers to one request.
pub struct Reply {
    status: &'static str,
    headers: String,
    body: Value,
    /// Whether it keeps the connection open without answering.
    silent: bool,
    /// Holds the reply back until it gets a message or its sender is dropped.
    held: Option<Receiver<()>>,
}

impl From<Value> for Reply {
    fn from(body: Value) -> Self {
        Self {
            status: "200 OK",
            headers: String::new(),
            body,
            silent: false,
            held: None,
        }
    }
}

/// No answer, as from a stuck API that takes the request and keeps the connection open.
pub fn no_answer() -> Reply {
    Reply {
        silent: true,
        ..Reply::from(Value::Null)
    }
}

/// `reply`, sent only once `release` gets a message or its sender is dropped, as a model
/// that takes its time answers.
pub fn held(reply: impl Into<Reply>, release: Receiver<()>) -> Reply {
    Reply {
        held: Some(release),
        ..reply.into()
    }
}

/// A refusal with 429 Too Many Requests and `message` in the body, as Aqueduct sends it, with a
/// `Retry-After` header if given.
pub fn too_many_requests(retry_after: Option<&str>, message: &str) -> Reply {
    Reply {
        status: "429 Too Many Requests",
        headers: retry_after.map_or(String::new(), |wait| format!("Retry-After: {wait}\r\n")),
        body: json!({ "error": { "message": message, "type": "rate_limit_error" } }),
        silent: false,
        held: None,
    }
}

/// What Aqueduct answered on 2026-10-25, with 400 Bad Request, to a request with an effort but
/// without `allowed_openai_params`.
pub const EFFORT_NOT_ALLOWED: &str = "aqueduct.UnsupportedParamsError: openai does not support \
    parameters: ['reasoning_effort'], for model=llm-d-qwen38/Qwen/Qwen3.8-27B-FP8. To drop these, \
    set `aqueduct.drop_params=True` or for proxy:\n\n`aqueduct_settings:\n drop_params: true`\n. \n \
    If you want to use these params dynamically send allowed_openai_params=['reasoning_effort'] in \
    your request.. Received Model Group=qwen-3.8-27b\nAvailable Model Group Fallbacks=None";

/// What Aqueduct answered on 2026-10-25, with 400 Bad Request, to `qwen-3.8-27b` at the effort
/// `high`, which Qwen 3.8's chat template refuses.
pub const EFFORT_REFUSED: &str = "aqueduct.BadRequestError: OpenAIException - Error code: 400 - \
    {'error': {'message': 'Unexpected reasoning effort high. Supported types are xhigh (default), \
    medium, and low.', 'type': 'BadRequestError', 'param': None, 'code': 400}}. Received Model \
    Group=qwen-3.8-27b\nAvailable Model Group Fallbacks=None";

/// A 307 Temporary Redirect to `location`, which asks for the same request there, body and all.
pub fn redirect(location: &str) -> Reply {
    Reply {
        status: "307 Temporary Redirect",
        headers: format!("Location: {location}\r\n"),
        body: Value::Null,
        silent: false,
        held: None,
    }
}

/// A refusal with 400 Bad Request and `message` in the body, as Aqueduct sends it.
pub fn bad_request(message: &str) -> Reply {
    failure("400 Bad Request", message)
}

/// An answer with `status`, such as `503 Service Unavailable`, and `message` in the body.
pub fn failure(status: &'static str, message: &str) -> Reply {
    Reply {
        status,
        headers: String::new(),
        body: json!({ "error": { "message": message, "type": "invalid_request_error" } }),
        silent: false,
        held: None,
    }
}

/// Answers like an OpenAI-compatible API with `replies`, one per request, and hands back the
/// requests it got once all replies are sent. The connections it gave no answer stay open until
/// then.
pub fn fake_model(replies: Vec<impl Into<Reply>>) -> (String, thread::JoinHandle<Vec<Request>>) {
    let replies: Vec<Reply> = replies.into_iter().map(Into::into).collect();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        let mut unanswered = Vec::new();
        for reply in replies {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body = if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body)
                    .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()))
            };
            requests.push((head, body));

            if reply.silent {
                unanswered.push(reader);
                continue;
            }
            if let Some(release) = &reply.held {
                let _ = release.recv();
            }
            let body = reply.body.to_string();
            write!(
                reader.get_mut(),
                "HTTP/1.1 {}\r\nContent-Type: application/json\r\n{}\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                reply.status,
                reply.headers,
                body.len()
            )
            .unwrap();
        }
        requests
    });
    (url, server)
}
