//! A fake model that answers like an OpenAI-compatible API, for tests here and in the crates
//! that enable the `testing` feature.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread;

use serde_json::{Value, json};

/// The head and the JSON body of a request the fake model received.
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

/// What the fake model answers to one request.
pub struct Reply {
    status: &'static str,
    headers: String,
    body: Value,
}

impl From<Value> for Reply {
    fn from(body: Value) -> Self {
        Self {
            status: "200 OK",
            headers: String::new(),
            body,
        }
    }
}

/// A refusal with 429 Too Many Requests and `message` in the body, as Aqueduct sends it, with a
/// `Retry-After` header if given.
pub fn too_many_requests(retry_after: Option<&str>, message: &str) -> Reply {
    Reply {
        status: "429 Too Many Requests",
        headers: retry_after.map_or(String::new(), |wait| format!("Retry-After: {wait}\r\n")),
        body: json!({ "error": { "message": message, "type": "rate_limit_error" } }),
    }
}

/// What Aqueduct answered on 2026-10-25, with 400 Bad Request, to a request with an effort but
/// without `allowed_openai_params`.
pub const EFFORT_NOT_ALLOWED: &str = "aqueduct.UnsupportedParamsError: openai does not support \
    parameters: ['reasoning_effort'], for model=llm-d-qwen38/Qwen/Qwen3.8-27B-FP8. To drop these, \
    set `aqueduct.drop_params=True` or for proxy:\n\n`aqueduct_settings:\n drop_params: true`\n. \n \
    If you want to use these params dynamically send allowed_openai_params=['reasoning_effort'] in \
    your request.. Received Model Group=qwen-3.8-27b\nAvailable Model Group Fallbacks=None";

/// A refusal with 400 Bad Request and `message` in the body, as Aqueduct sends it.
pub fn bad_request(message: &str) -> Reply {
    Reply {
        status: "400 Bad Request",
        headers: String::new(),
        body: json!({ "error": { "message": message, "type": "invalid_request_error" } }),
    }
}

/// Answers like an OpenAI-compatible API with `replies`, one per request, and hands back the
/// requests it got once all replies are sent.
pub fn fake_model(replies: Vec<impl Into<Reply>>) -> (String, thread::JoinHandle<Vec<Request>>) {
    let replies: Vec<Reply> = replies.into_iter().map(Into::into).collect();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
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
                serde_json::from_slice(&body).unwrap()
            };
            requests.push((head, body));

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
