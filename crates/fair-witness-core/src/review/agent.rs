use std::path::Path;
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result, bail};
use genai::Client;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ReasoningEffort, Tool, ToolChoice, ToolResponse,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::runtime::Runtime;

use super::prompt::{self, SYSTEM_PROMPT};
use super::provider::openai_compatible;
use super::tools::Tools;
use crate::project::{AiConfig, Effort};
use crate::watcher::Change;

const MAX_TURNS: usize = 12;

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
        let prompt = prompt::user_message(dmp, changes);
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::review::fake_model::fake_model;

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

    fn reviewer(url: String) -> Reviewer {
        Reviewer::new(
            &AiConfig {
                url,
                model: "test".to_string(),
                effort: Some(Effort::Low),
            },
            Some("sk-test".to_string()),
        )
        .unwrap()
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
        let reviewer = reviewer(endpoint);

        let mut traces = Vec::new();
        let verdict = reviewer
            .review(
                root.path(),
                r#"{"dmp": {"dataset": [{"personal_data": "no"}]}}"#,
                &[Change::Created("data/patients.csv".into())],
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
    fn sends_a_long_change_set_per_folder() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, server) = fake_model(vec![tool_call(
            "report_verdict",
            json!({ "contradicts": false, "explanation": "Survey answers, as planned." }),
        )]);
        let changes: Vec<Change> = (0..60)
            .map(|i| Change::Created(format!("data/raw/{i}.csv").into()))
            .collect();

        reviewer(endpoint)
            .review(root.path(), "{}", &changes, &mut |_| {})
            .unwrap();

        let body = &server.join().unwrap()[0].1;
        let prompt = body["messages"][1]["content"].as_str().unwrap();
        assert!(
            prompt.ends_with(
                "What changed in the project folder:\n\
                 created: 60 paths under data/raw/\n\n\
                 There were too many changes to name each path. list_dir shows the paths \
                 that still exist; deleted ones are gone."
            ),
            "{prompt}"
        );
    }
}
