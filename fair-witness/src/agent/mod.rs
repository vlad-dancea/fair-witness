mod tools;

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result, bail};
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ReasoningEffort, Tool, ToolChoice, ToolResponse,
};
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ModelIden, ServiceTarget};
use reqwest::blocking::Client as HttpClient;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::runtime::Runtime;

use crate::config::{AiConfig, Effort};
use crate::watcher::Change;
use tools::Tools;

const MAX_TURNS: usize = 12;

const SYSTEM_PROMPT: &str = "\
You check a researcher's project folder against their data management plan (DMP).
You get the DMP as maDMP JSON (RDA DMP Common Standard) and a list of files that
just changed in the folder. Decide whether the changes contradict the DMP: for
example data of a kind, format, size or sensitivity the DMP does not describe,
personal data where the DMP says there is none, or files in a format the DMP
rules out. Changes the DMP does not cover at all (notes, scripts, drafts) do not
contradict it.

Use list_dir and read_file to look at the changed files when their names alone
do not settle it. Be frugal: read only what you need. Then call report_verdict
exactly once. Only report a contradiction you can point to in the DMP.";

#[derive(Debug, Deserialize)]
pub struct Verdict {
    pub contradicts: bool,
    pub dmp_field: Option<String>,
    pub explanation: String,
}

/// One step of a review, for the debug pane.
#[derive(Debug, Clone)]
pub enum Trace {
    /// A message added to the conversation by us.
    Sent {
        role: &'static str,
        text: String,
    },
    /// The conversation so far goes to the model.
    Request {
        turn: usize,
        choice: ModelChoice,
        tools: Vec<String>,
    },
    Reply {
        turn: usize,
        reasoning: Option<String>,
        texts: Vec<String>,
        calls: Vec<(String, Value)>,
        stop_reason: Option<String>,
        tokens_in: Option<i32>,
        tokens_out: Option<i32>,
    },
    ToolResult {
        name: String,
        output: String,
    },
    Failed(String),
}

pub struct Reviewer {
    runtime: Runtime,
    client: Client,
    choice: Mutex<ModelChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub model: String,
    pub effort: Option<Effort>,
}

impl Reviewer {
    pub fn new(config: &AiConfig, api_key: Option<String>) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            runtime,
            client: openai_compatible(&config.url, api_key),
            choice: Mutex::new(ModelChoice {
                model: config.model.clone(),
                effort: config.effort,
            }),
        })
    }

    pub fn choice(&self) -> ModelChoice {
        self.choice
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set_choice(&self, choice: ModelChoice) {
        *self.choice.lock().unwrap_or_else(PoisonError::into_inner) = choice;
    }

    pub fn review(
        &self,
        root: &Path,
        dmp: &str,
        changes: &[Change],
        trace: &mut dyn FnMut(Trace),
    ) -> Result<Verdict> {
        let tools = Tools::new(root)?;
        let relative = |path: &Path| {
            path.strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string()
        };
        let changes: Vec<String> = changes
            .iter()
            .map(|change| match change {
                Change::Created(path) => format!("created: {}", relative(path)),
                Change::Changed(path) => format!("changed: {}", relative(path)),
                Change::Renamed { from, to } => {
                    format!("renamed: {} -> {}", relative(from), relative(to))
                }
                Change::Deleted(path) => format!("deleted: {}", relative(path)),
            })
            .collect();
        let prompt = format!(
            "The DMP:\n```json\n{dmp}\n```\n\nWhat changed in the project folder:\n{}",
            changes.join("\n")
        );
        self.runtime
            .block_on(self.run(&self.choice(), &tools, prompt, trace))
    }

    async fn run(
        &self,
        choice: &ModelChoice,
        tools: &Tools,
        prompt: String,
        trace: &mut dyn FnMut(Trace),
    ) -> Result<Verdict> {
        let mut definitions = Tools::definitions();
        definitions.push(verdict_tool());
        let tool_names: Vec<String> = definitions
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        trace(Trace::Sent {
            role: "system",
            text: SYSTEM_PROMPT.to_string(),
        });
        trace(Trace::Sent {
            role: "user",
            text: prompt.clone(),
        });
        let mut request = ChatRequest::new(vec![ChatMessage::user(prompt)])
            .with_system(SYSTEM_PROMPT)
            .with_tools(definitions);
        let mut options = ChatOptions::default().with_tool_choice(ToolChoice::Auto);
        if let Some(effort) = choice.effort {
            options = options
                .with_reasoning_effort(reasoning_effort(effort))
                .with_extra_body(json!({ "allowed_openai_params": ["reasoning_effort"] }));
        }

        for turn in 1..=MAX_TURNS {
            trace(Trace::Request {
                turn,
                choice: choice.clone(),
                tools: tool_names.clone(),
            });
            let response = match self
                .client
                .exec_chat(&choice.model, request.clone(), Some(&options))
                .await
            {
                Ok(response) => response,
                Err(e) => {
                    trace(Trace::Failed(format!("{e:#}")));
                    return Err(e).with_context(|| format!("the model {} failed", choice.model));
                }
            };
            let calls: Vec<_> = response.tool_calls().into_iter().cloned().collect();
            trace(Trace::Reply {
                turn,
                reasoning: response.reasoning_content.clone(),
                texts: response.texts().into_iter().map(str::to_string).collect(),
                calls: calls
                    .iter()
                    .map(|call| (call.fn_name.clone(), call.fn_arguments.clone()))
                    .collect(),
                stop_reason: response.stop_reason.as_ref().map(|r| r.raw().to_string()),
                tokens_in: response.usage.prompt_tokens,
                tokens_out: response.usage.completion_tokens,
            });
            request = request.append_message(ChatMessage::assistant(response.content));
            if calls.is_empty() {
                let nudge = "Please finish by calling report_verdict.";
                trace(Trace::Sent {
                    role: "user",
                    text: nudge.to_string(),
                });
                request = request.append_message(ChatMessage::user(nudge));
                continue;
            }
            for call in calls {
                let output = if call.fn_name == "report_verdict" {
                    match serde_json::from_value(call.fn_arguments.clone()) {
                        Ok(verdict) => return Ok(verdict),
                        Err(e) => format!("error: invalid verdict: {e}"),
                    }
                } else {
                    tools
                        .call(&call.fn_name, &call.fn_arguments)
                        .unwrap_or_else(|e| format!("error: {e}"))
                };
                trace(Trace::ToolResult {
                    name: call.fn_name.clone(),
                    output: output.clone(),
                });
                request = request.append_message(ToolResponse::new(call.call_id, output));
            }
        }
        bail!("the model gave no verdict after {MAX_TURNS} replies")
    }
}

fn reasoning_effort(effort: Effort) -> ReasoningEffort {
    match effort {
        Effort::None => ReasoningEffort::None,
        Effort::Low => ReasoningEffort::Low,
        Effort::Medium => ReasoningEffort::Medium,
        Effort::High => ReasoningEffort::High,
    }
}

fn verdict_tool() -> Tool {
    Tool::new("report_verdict")
        .with_description("Hand in the verdict. Ends the review.")
        .with_schema(json!({
            "type": "object",
            "properties": {
                "contradicts": {
                    "type": "boolean",
                    "description": "Whether the changes contradict the DMP."
                },
                "dmp_field": {
                    "type": "string",
                    "description": "If they do: the DMP field they contradict, e.g. `dataset[0].personal_data`."
                },
                "explanation": {
                    "type": "string",
                    "description": "One or two sentences for the researcher."
                }
            },
            "required": ["contradicts", "explanation"]
        }))
}

pub fn list_models(http: &HttpClient, url: &str, api_key: Option<&str>) -> Result<Vec<String>> {
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
        .with_context(|| format!("could not reach {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("{url} answered {status}; is the URL right, and the API key?");
    }
    let models: Models = response
        .json()
        .with_context(|| format!("{url} does not look like an OpenAI-compatible API"))?;
    let mut ids: Vec<String> = models.data.into_iter().map(|model| model.id).collect();
    ids.sort();
    Ok(ids)
}

fn base_url(url: &str) -> String {
    format!("{}/", url.trim_end_matches('/'))
}

fn openai_compatible(url: &str, api_key: Option<String>) -> Client {
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use serde_json::Value;

    use super::*;

    type Request = (String, Value);

    fn fake_model(replies: Vec<Value>) -> (String, thread::JoinHandle<Vec<Request>>) {
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

                let body = reply.to_string();
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            requests
        });
        (url, server)
    }

    fn tool_call(name: &str, arguments: Value) -> Value {
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

    #[test]
    fn runs_tools_until_the_model_reports_a_verdict() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("data")).unwrap();
        fs::write(root.path().join("data/patients.csv"), "name,diagnosis\n").unwrap();
        let (endpoint, server) = fake_model(vec![
            tool_call("read_file", json!({ "path": "data/patients.csv" })),
            tool_call(
                "report_verdict",
                json!({
                    "contradicts": true,
                    "dmp_field": "dataset[0].personal_data",
                    "explanation": "Patient names, but the DMP says no personal data."
                }),
            ),
        ]);
        let reviewer = Reviewer::new(
            &AiConfig {
                url: endpoint,
                model: "test".to_string(),
                effort: Some(Effort::Low),
            },
            Some("sk-test".to_string()),
        )
        .unwrap();

        let mut traces = Vec::new();
        let verdict = reviewer
            .review(
                root.path(),
                r#"{"dmp": {"dataset": [{"personal_data": "no"}]}}"#,
                &[Change::Created(root.path().join("data/patients.csv"))],
                &mut |trace| traces.push(trace),
            )
            .unwrap();

        assert!(verdict.contradicts);
        assert_eq!(
            verdict.dmp_field.as_deref(),
            Some("dataset[0].personal_data")
        );
        let requests = server.join().unwrap();
        let (head, body) = &requests[0];
        assert!(head.starts_with("POST /v1/chat/completions "), "{head}");
        assert!(head.contains("authorization: Bearer sk-test"), "{head}");
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["allowed_openai_params"], json!(["reasoning_effort"]));
        let first = body.to_string();
        assert!(first.contains("created: data/patients.csv"), "{first}");
        let messages = requests[1].1["messages"].as_array().unwrap();
        let tool_message = messages.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool_message["content"], "name,diagnosis\n");

        let kinds: Vec<String> = traces
            .iter()
            .map(|trace| match trace {
                Trace::Sent { role, .. } => format!("sent {role}"),
                Trace::Request { turn, .. } => format!("request {turn}"),
                Trace::Reply { turn, calls, .. } => format!("reply {turn}: {}", calls[0].0),
                Trace::ToolResult { name, output } => format!("{name}: {output}"),
                Trace::Failed(e) => format!("failed: {e}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "sent system",
                "sent user",
                "request 1",
                "reply 1: read_file",
                "read_file: name,diagnosis\n",
                "request 2",
                "reply 2: report_verdict",
            ]
        );
    }

    #[test]
    fn lists_the_models_a_server_offers() {
        let (url, server) = fake_model(vec![json!({
            "object": "list",
            "data": [
                { "id": "qwen3:8b", "object": "model" },
                { "id": "llama3.2", "object": "model" }
            ]
        })]);

        let models = list_models(&HttpClient::new(), &format!("{url}/"), Some("sk-test")).unwrap();

        assert_eq!(models, ["llama3.2", "qwen3:8b"]);
        let (head, _) = &server.join().unwrap()[0];
        assert!(head.starts_with("GET /v1/models "), "{head}");
        assert!(head.contains("authorization: Bearer sk-test"), "{head}");
    }
}
