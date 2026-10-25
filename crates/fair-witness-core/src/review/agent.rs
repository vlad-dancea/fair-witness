use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use genai::Client;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatResponse, ReasoningEffort, Tool, ToolChoice,
    ToolResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::prompt::{self, SYSTEM_PROMPT};
use super::provider::{openai_compatible, too_many_requests};
use super::steps::{Input, Outcome, Step, StepFuture};
use super::tools::Tools;
use crate::project::{AiConfig, Effort};

const MAX_TURNS: usize = 12;
/// How often a request goes again after the API refused it with 429 Too Many Requests. Reviews
/// that share a model race for each request its limit frees, so one can lose several times.
const MAX_RETRIES: usize = 10;
/// The longest wait before a retry. A limit per minute never asks for more, and a review is not
/// worth holding for a longer one, such as a daily quota.
const MAX_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub contradicts: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dmp_field: Option<String>,
    pub explanation: String,
}

/// What happened in a review, for the log and the debug pane. Fields without a value are left
/// out of the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trace {
    /// What the model is told before any change: the system prompt, the DMP and the tools it
    /// may call. A session passes it on only when it differs from the last one.
    Setup {
        system: String,
        dmp: String,
        tools: Vec<String>,
    },
    /// The model the review asks. A session passes it on only when it differs from the last one.
    Model(ModelChoice),
    /// The first message of a review without the DMP, which `first_message` puts before it.
    Prompt(String),
    /// A later message from us, such as a reminder to hand in the verdict.
    Sent(String),
    /// The conversation so far goes to the model.
    Request {
        turn: usize,
    },
    /// The API refused request `turn` with 429 Too Many Requests, so it goes again after
    /// `wait_ms`, as retry number `retry`.
    RateLimited {
        turn: usize,
        retry: usize,
        wait_ms: u64,
        /// What the API answered.
        body: String,
    },
    Reply {
        turn: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        texts: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        calls: Vec<(String, Value)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_in: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_out: Option<i32>,
        /// How long the model took to reply.
        elapsed_ms: u64,
    },
    ToolResult {
        name: String,
        output: String,
        /// Whether the call failed, so that `output` is the error the model is told, which a
        /// file can look like.
        #[serde(default, skip_serializing_if = "is_false")]
        failed: bool,
    },
    Failed(String),
}

fn is_false(value: &bool) -> bool {
    !value
}

pub struct Reviewer {
    client: Client,
    choice: Mutex<ModelChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
        let about_changes = prompt::about_changes(input.changes, input.found);
        let choice = self.choice();
        let mut definitions = Tools::definitions();
        definitions.push(verdict_tool());
        trace(Trace::Setup {
            system: SYSTEM_PROMPT.to_string(),
            dmp: input.dmp.to_string(),
            tools: definitions
                .iter()
                .map(|tool| tool.name.to_string())
                .collect(),
        });
        trace(Trace::Model(choice.clone()));
        trace(Trace::Prompt(about_changes.clone()));
        let first = prompt::first_message(input.dmp, &about_changes);
        let mut request = ChatRequest::new(vec![ChatMessage::user(first)])
            .with_system(SYSTEM_PROMPT)
            .with_tools(definitions);
        let mut options = ChatOptions::default().with_tool_choice(ToolChoice::Auto);
        if let Some(effort) = choice.effort {
            options = options
                .with_reasoning_effort(reasoning_effort(effort))
                .with_extra_body(json!({ "allowed_openai_params": ["reasoning_effort"] }));
        }

        for turn in 1..=MAX_TURNS {
            let (response, elapsed) = self
                .ask(&choice.model, &request, &options, turn, trace)
                .await?;
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
                elapsed_ms: millis(elapsed),
            });
            request = request.append_message(ChatMessage::assistant(response.content));
            if calls.is_empty() {
                let nudge = "Please finish by calling report_verdict.";
                trace(Trace::Sent(nudge.to_string()));
                request = request.append_message(ChatMessage::user(nudge));
                continue;
            }
            for call in calls {
                let result = if call.fn_name == "report_verdict" {
                    match serde_json::from_value(call.fn_arguments.clone()) {
                        Ok(verdict) => return Ok(verdict),
                        Err(e) => Err(format!("invalid verdict: {e}")),
                    }
                } else {
                    // The tools read the disk, which can block, such as on a FIFO.
                    let tools = tools.clone();
                    let (name, arguments) = (call.fn_name.clone(), call.fn_arguments.clone());
                    tokio::task::spawn_blocking(move || tools.call(&name, &arguments))
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                };
                let failed = result.is_err();
                let output = result.unwrap_or_else(|e| format!("error: {e}"));
                trace(Trace::ToolResult {
                    name: call.fn_name.clone(),
                    output: output.clone(),
                    failed,
                });
                request = request.append_message(ToolResponse::new(call.call_id, output));
            }
        }
        bail!("the model gave no verdict after {MAX_TURNS} replies")
    }

    /// Sends `request` and gives back the reply with how long it took. When the API refuses the
    /// request with 429 Too Many Requests, waits as long as it asks and sends it again.
    async fn ask(
        &self,
        model: &str,
        request: &ChatRequest,
        options: &ChatOptions,
        turn: usize,
        trace: &(dyn Fn(Trace) + Sync),
    ) -> Result<(ChatResponse, Duration)> {
        let mut retry = 0;
        loop {
            trace(Trace::Request { turn });
            let asked = Instant::now();
            let e = match self
                .client
                .exec_chat(model, request.clone(), Some(options))
                .await
            {
                Ok(response) => return Ok((response, asked.elapsed())),
                Err(e) => e,
            };
            let failed = match too_many_requests(&e) {
                None => format!("the model {model} failed"),
                Some(_) if retry == MAX_RETRIES => format!(
                    "the model {model} still answered 429 Too Many Requests after \
                     {MAX_RETRIES} retries"
                ),
                Some(refused) => {
                    // Without a hint, the waits grow from 2 seconds.
                    let wait = refused
                        .retry_after
                        .unwrap_or_else(|| Duration::from_secs(2 << retry.min(5)).min(MAX_WAIT));
                    if wait > MAX_WAIT {
                        format!(
                            "the model {model} answered 429 Too Many Requests and asks to wait \
                             {} s, longer than fair-witness waits ({} s)",
                            wait.as_secs(),
                            MAX_WAIT.as_secs()
                        )
                    } else {
                        retry += 1;
                        trace(Trace::RateLimited {
                            turn,
                            retry,
                            wait_ms: millis(wait),
                            body: refused.body,
                        });
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                }
            };
            trace(Trace::Failed(format!("{e:#}")));
            return Err(e).context(failed);
        }
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

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
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
    use crate::testing::{Reply, fake_model, too_many_requests, tool_call};
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

        let traces = traces.into_inner().unwrap();
        let (Trace::Setup { system, dmp, .. }, Trace::Prompt(about_changes)) =
            (&traces[0], &traces[2])
        else {
            panic!("no setup and prompt first: {traces:?}");
        };
        assert_eq!(body["messages"][0]["content"], json!(system));
        assert_eq!(
            body["messages"][1]["content"],
            json!(prompt::first_message(dmp, about_changes))
        );
        let kinds: Vec<String> = traces
            .iter()
            .map(|trace| match trace {
                Trace::Setup { tools, .. } => format!("setup with {}", tools.join(", ")),
                Trace::Model(choice) => format!("model {}", choice.model),
                Trace::Prompt(text) => format!("prompt {}", text.lines().nth(1).unwrap_or("")),
                Trace::Sent(text) => format!("sent {text}"),
                Trace::Request { turn } => format!("request {turn}"),
                Trace::RateLimited { turn, .. } => format!("request {turn} rate limited"),
                Trace::Reply { turn, calls, .. } => format!("reply {turn}: {}", calls[0].0),
                Trace::ToolResult { name, output, .. } => format!("{name}: {output}"),
                Trace::Failed(e) => format!("failed: {e}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "setup with list_dir, read_file, report_verdict",
                "model test",
                "prompt created: data/patients.csv",
                "request 1",
                "reply 1: read_file",
                "read_file: name,diagnosis\n",
                "request 2",
                "reply 2: report_verdict",
            ]
        );
    }

    #[tokio::test]
    async fn marks_the_tool_calls_that_fail() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("log.txt"), "error: disk full\n").unwrap();
        let (endpoint, server) = fake_model(vec![
            tool_call("read_file", json!({ "path": "log.txt" })),
            tool_call("read_flie", json!({ "path": "log.txt" })),
            tool_call("report_verdict", json!({ "contradicts": "no" })),
            tool_call(
                "report_verdict",
                json!({ "contradicts": false, "explanation": "A log file." }),
            ),
        ]);

        let traces = Mutex::new(Vec::new());
        reviewer(endpoint)
            .review(
                &input(root.path(), "{}", &[Change::Created("log.txt".into())]),
                &|trace| traces.lock().unwrap().push(trace),
            )
            .await
            .unwrap();

        server.join().unwrap();
        let results: Vec<(String, String, bool)> = traces
            .into_inner()
            .unwrap()
            .into_iter()
            .filter_map(|trace| match trace {
                Trace::ToolResult {
                    name,
                    output,
                    failed,
                } => Some((name, output, failed)),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            [
                ("read_file".into(), "error: disk full\n".into(), false),
                (
                    "read_flie".into(),
                    "error: unknown tool `read_flie`".into(),
                    true
                ),
                (
                    "report_verdict".into(),
                    "error: invalid verdict: invalid type: string \"no\", expected a boolean"
                        .into(),
                    true
                ),
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

    fn verdict() -> Value {
        tool_call(
            "report_verdict",
            json!({ "contradicts": false, "explanation": "A log file." }),
        )
    }

    #[tokio::test]
    async fn waits_as_long_as_the_api_asks_and_sends_the_request_again() {
        let root = tempfile::tempdir().unwrap();
        let refused = "Rate limit exceeded. Request limit (25/min) (Retry after 1s).";
        let (endpoint, server) =
            fake_model(vec![too_many_requests(None, refused), verdict().into()]);

        let traces = Mutex::new(Vec::new());
        let asked = Instant::now();
        let verdict = reviewer(endpoint)
            .review(
                &input(root.path(), "{}", &[Change::Created("log.txt".into())]),
                &|trace| traces.lock().unwrap().push(trace),
            )
            .await
            .unwrap();

        assert!(
            asked.elapsed() >= Duration::from_secs(1),
            "{:?}",
            asked.elapsed()
        );
        assert_eq!(verdict.explanation, "A log file.");
        let requests = server.join().unwrap();
        assert_eq!(requests[1].1, requests[0].1);
        let traces: Vec<Trace> = traces
            .into_inner()
            .unwrap()
            .into_iter()
            .filter(|trace| matches!(trace, Trace::Request { .. } | Trace::RateLimited { .. }))
            .collect();
        assert_eq!(
            traces,
            [
                Trace::Request { turn: 1 },
                Trace::RateLimited {
                    turn: 1,
                    retry: 1,
                    wait_ms: 1000,
                    body: json!({ "error": { "message": refused, "type": "rate_limit_error" } })
                        .to_string(),
                },
                Trace::Request { turn: 1 },
            ]
        );
    }

    #[tokio::test]
    async fn fails_once_the_retries_run_out() {
        let root = tempfile::tempdir().unwrap();
        let replies: Vec<Reply> = (0..=MAX_RETRIES)
            .map(|_| too_many_requests(Some("0"), "Rate limit exceeded."))
            .collect();
        let (endpoint, server) = fake_model(replies);

        let traces = Mutex::new(Vec::new());
        let e = reviewer(endpoint)
            .review(&input(root.path(), "{}", &[]), &|trace| {
                traces.lock().unwrap().push(trace)
            })
            .await
            .unwrap_err();

        assert_eq!(
            e.to_string(),
            "the model test still answered 429 Too Many Requests after 10 retries"
        );
        assert_eq!(server.join().unwrap().len(), MAX_RETRIES + 1);
        let traces = traces.into_inner().unwrap();
        let retries: Vec<(usize, u64)> = traces
            .iter()
            .filter_map(|trace| match trace {
                Trace::RateLimited { retry, wait_ms, .. } => Some((*retry, *wait_ms)),
                _ => None,
            })
            .collect();
        let expected: Vec<(usize, u64)> = (1..=MAX_RETRIES).map(|retry| (retry, 0)).collect();
        assert_eq!(retries, expected);
        assert!(
            matches!(traces.last(), Some(Trace::Failed(e)) if e.contains("429 Too Many Requests")),
            "{traces:?}"
        );
    }

    #[tokio::test]
    async fn fails_at_once_when_the_api_asks_to_wait_too_long() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, server) = fake_model(vec![too_many_requests(
            Some("86400"),
            "Daily quota exceeded.",
        )]);

        let e = reviewer(endpoint)
            .review(&input(root.path(), "{}", &[]), &|_| {})
            .await
            .unwrap_err();

        assert_eq!(
            e.to_string(),
            "the model test answered 429 Too Many Requests and asks to wait 86400 s, longer \
             than fair-witness waits (60 s)"
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
