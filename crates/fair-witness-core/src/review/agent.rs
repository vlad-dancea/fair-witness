use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use genai::Client;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ReasoningEffort, Tool, ToolChoice, ToolResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::prompt::{self, SYSTEM_PROMPT};
use super::provider::openai_compatible;
use super::steps::{Input, Outcome, Step, StepFuture};
use super::tools::Tools;
use crate::project::{AiConfig, Effort};

const MAX_TURNS: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub contradicts: bool,
    pub dmp_field: Option<String>,
    pub explanation: String,
}

/// What happened in a review, for the debug pane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trace {
    /// A message added to the conversation by us.
    Sent {
        role: String,
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
        /// How long the model took to reply.
        elapsed_ms: u64,
    },
    ToolResult {
        name: String,
        output: String,
    },
    Failed(String),
}

pub struct Reviewer {
    client: Client,
    choice: Mutex<ModelChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    pub model: String,
    pub effort: Option<Effort>,
}

impl Reviewer {
    pub fn new(config: &AiConfig, api_key: Option<String>) -> Self {
        Self {
            client: openai_compatible(&config.url, api_key),
            choice: Mutex::new(ModelChoice {
                model: config.model.clone(),
                effort: config.effort,
            }),
        }
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

    pub async fn review(
        &self,
        input: &Input<'_>,
        trace: &(dyn Fn(Trace) + Sync),
    ) -> Result<Verdict> {
        let tools = Arc::new(Tools::new(input.root)?);
        let prompt = prompt::user_message(input.dmp, input.changes, input.found);
        let choice = self.choice();
        let mut definitions = Tools::definitions();
        definitions.push(verdict_tool());
        let tool_names: Vec<String> = definitions
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        trace(Trace::Sent {
            role: "system".to_string(),
            text: SYSTEM_PROMPT.to_string(),
        });
        trace(Trace::Sent {
            role: "user".to_string(),
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
            let asked = Instant::now();
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
                elapsed_ms: u64::try_from(asked.elapsed().as_millis()).unwrap_or(u64::MAX),
            });
            request = request.append_message(ChatMessage::assistant(response.content));
            if calls.is_empty() {
                let nudge = "Please finish by calling report_verdict.";
                trace(Trace::Sent {
                    role: "user".to_string(),
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
                    // The tools read the disk, which can block, such as on a FIFO.
                    let tools = tools.clone();
                    let (name, arguments) = (call.fn_name.clone(), call.fn_arguments.clone());
                    tokio::task::spawn_blocking(move || tools.call(&name, &arguments))
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
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

impl Step for Reviewer {
    fn check<'a>(
        &'a self,
        input: &'a Input<'a>,
        trace: &'a (dyn Fn(Trace) + Sync),
    ) -> StepFuture<'a> {
        Box::pin(async move { Ok(Outcome::Decided(self.review(input, trace).await?)) })
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
    use crate::testing::{fake_model, tool_call};
    use crate::watcher::Change;

    fn input<'a>(root: &'a std::path::Path, dmp: &'a str, changes: &'a [Change]) -> Input<'a> {
        Input {
            root,
            dmp,
            changes,
            found: &[],
        }
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
    }

    #[tokio::test]
    async fn runs_tools_until_the_model_reports_a_verdict() {
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

        let traces = Mutex::new(Vec::new());
        let verdict = reviewer
            .review(
                &input(
                    root.path(),
                    r#"{"dmp": {"dataset": [{"personal_data": "no"}]}}"#,
                    &[Change::Created("data/patients.csv".into())],
                ),
                &|trace| traces.lock().unwrap().push(trace),
            )
            .await
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
            .into_inner()
            .unwrap()
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

    #[tokio::test]
    async fn sends_a_long_change_set_per_folder() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, server) = fake_model(vec![tool_call(
            "report_verdict",
            json!({ "contradicts": false, "explanation": "Survey answers, as planned." }),
        )]);
        let changes: Vec<Change> = (0..60)
            .map(|i| Change::Created(format!("data/raw/{i}.csv").into()))
            .collect();

        reviewer(endpoint)
            .review(&input(root.path(), "{}", &changes), &|_| {})
            .await
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
