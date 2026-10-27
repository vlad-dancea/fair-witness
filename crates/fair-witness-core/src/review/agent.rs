use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use genai::Client;
use genai::chat::{
    CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatResponse, ReasoningEffort, Tool,
    ToolChoice, ToolResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::prompt::{self, SYSTEM_PROMPT};
use super::provider::{
    asks_to_allow_effort, openai_compatible, refused, too_many_requests, why_failed,
};
use super::steps::{Input, Outcome, Step, StepFuture};
use super::tools::Tools;
use crate::project::{AiConfig, Effort};
use crate::secret;

const MAX_TURNS: usize = 12;
/// How often a request goes again after the API refused it with 429 Too Many Requests. Reviews
/// that share a model race for each request its limit frees, so one can lose several times.
const MAX_RETRIES: usize = 10;
/// The longest wait before a retry. A limit per minute never asks for more, and a review is not
/// worth holding for a longer one, such as a daily quota.
const MAX_WAIT: Duration = Duration::from_secs(60);
/// How long a check waits for the API: long enough for a busy one to answer with one token, and
/// short enough that `run` does not seem to hang when it starts.
const CHECK_WAIT: Duration = Duration::from_secs(30);
/// What a check sends in place of the DMP and the changes.
const CHECK_MESSAGE: &str = "Nothing changed. This request only checks that the API takes the \
                             model, its effort and the tools.";
/// The most characters of what an API answered that a trace, an error or a check keeps. A
/// gateway may answer with a whole web page.
const MAX_ANSWER: usize = 1_000;

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
    Request { turn: usize },
    /// The API refused request `turn` with 429 Too Many Requests, so it goes again after
    /// `wait_ms`, as retry number `retry`.
    RateLimited {
        turn: usize,
        retry: usize,
        wait_ms: u64,
        /// What the API answered, as `Reviewer::reportable` leaves it.
        body: String,
    },
    /// The API refused request `turn` and asked to name its effort in `allowed_openai_params`,
    /// as LiteLLM does, so it goes again with the field, which later requests to the model
    /// carry from the start.
    EffortNotAllowed {
        turn: usize,
        /// What the API answered, as `Reviewer::reportable` leaves it.
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
    /// Why a request failed, in one line as `Reviewer::reportable` leaves it.
    Failed(String),
}

fn is_false(value: &bool) -> bool {
    !value
}

pub struct Reviewer {
    client: Client,
    choice: Mutex<ModelChoice>,
    /// The models the API asked to send `allowed_openai_params` with an effort for. LiteLLM
    /// asks for each model it does not know to take an effort, and the field would make it
    /// pass an effort on raw to one it does know, such as Anthropic's, which refuses it.
    allowing_effort: Mutex<HashSet<String>>,
    /// Kept to hide it in what a check or a review reports, since an API may repeat a key it
    /// refuses.
    api_key: Option<String>,
}

/// What the API made of a model and effort when a check asked it once, as a review would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    Accepted,
    /// The API refused the request with a 4xx status other than 408 Request Timeout, 409
    /// Conflict and 429 Too Many Requests, and said this.
    Refused(String),
    /// The check could not tell, for this reason, such as an API that could not be reached,
    /// answered that it is busy with 408, 409 or 429, or failed on its side.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    pub model: String,
    #[serde(default, skip_serializing_if = "Effort::is_default")]
    pub effort: Effort,
}

impl Reviewer {
    pub fn new(config: &AiConfig, api_key: Option<String>) -> Result<Self> {
        Ok(Self {
            client: openai_compatible(&config.url, api_key.clone())?,
            choice: Mutex::new(ModelChoice {
                model: config.model.clone(),
                effort: config.effort.clone(),
            }),
            allowing_effort: Mutex::default(),
            api_key,
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

    pub async fn review(
        &self,
        input: &Input<'_>,
        trace: &(dyn Fn(Trace) + Sync),
    ) -> Result<Verdict> {
        let tools = Arc::new(Tools::new(input.root, input.filter.clone())?);
        let about_changes = prompt::about_changes(input.changes, input.found);
        let choice = self.choice();
        let definitions = definitions();
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
        let options = options(&choice.effort);

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

    /// Asks the API once as a review would, with the system prompt, the tools and the effort of
    /// `choice` but at most one token of output, so that a model or effort it refuses shows when
    /// it is chosen instead of failing every review.
    pub async fn check(&self, choice: &ModelChoice) -> Checked {
        let request = ChatRequest::new(vec![ChatMessage::user(CHECK_MESSAGE)])
            .with_system(SYSTEM_PROMPT)
            .with_tools(definitions());
        let options = options(&choice.effort);
        let asked = async {
            let limited = options.clone().with_max_tokens(1);
            match checked(
                self.send(&choice.model, &request, &limited, 1, &|_| {})
                    .await,
            ) {
                // The API may refuse the limit itself, such as OpenAI's newer reasoning models the
                // `max_tokens` that genai sends for model names it does not know, so only a request
                // without it can show a refusal of the model or effort. An API refuses before the
                // model writes anything, so the second request costs tokens only when the limit
                // was what it refused.
                Checked::Refused(_) => checked(
                    self.send(&choice.model, &request, &options, 1, &|_| {})
                        .await,
                ),
                answered => answered,
            }
        };
        let checked = tokio::time::timeout(CHECK_WAIT, asked)
            .await
            .unwrap_or_else(|_| {
                Checked::Unknown(format!(
                    "the API did not answer within {} s",
                    CHECK_WAIT.as_secs()
                ))
            });
        match checked {
            Checked::Accepted => Checked::Accepted,
            Checked::Refused(message) => Checked::Refused(self.reportable(message)),
            Checked::Unknown(why) => Checked::Unknown(self.reportable(why)),
        }
    }

    /// `text`, from the API or about it, without the API key and cut to `MAX_ANSWER`
    /// characters. The key goes first, so that the cut cannot leave part of it.
    fn reportable(&self, text: String) -> String {
        let text = secret::hide(text, self.api_key.as_deref());
        match text.char_indices().nth(MAX_ANSWER) {
            Some((end, _)) => format!("{}…", &text[..end]),
            None => text,
        }
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
            let e = match self.send(model, request, options, turn, trace).await {
                Ok(replied) => return Ok(replied),
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
                            body: self.reportable(refused.body),
                        });
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                }
            };
            let why = self.reportable(why_failed(&e));
            trace(Trace::Failed(why.clone()));
            return Err(anyhow!(why).context(failed));
        }
    }

    /// Sends `request` and gives back the reply with how long it took. When the API asks for
    /// `allowed_openai_params`, sends it again with the field.
    async fn send(
        &self,
        model: &str,
        request: &ChatRequest,
        options: &ChatOptions,
        turn: usize,
        trace: &(dyn Fn(Trace) + Sync),
    ) -> genai::Result<(ChatResponse, Duration)> {
        // The options always carry a budget, so whether the request names an effort shows only
        // in the extra body.
        let effort = options
            .extra_body
            .as_ref()
            .is_some_and(|body| body.get("reasoning_effort").is_some());
        loop {
            let allowing = effort
                && self
                    .allowing_effort
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .contains(model);
            let options = if allowing {
                with_effort_allowed(options)
            } else {
                options.clone()
            };
            trace(Trace::Request { turn });
            let asked = Instant::now();
            let e = match self
                .client
                .exec_chat(model, request.clone(), Some(&options))
                .await
            {
                Ok(response) => return Ok((response, asked.elapsed())),
                Err(e) => e,
            };
            if effort
                && !allowing
                && let Some(body) = asks_to_allow_effort(&e)
            {
                self.allowing_effort
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(model.to_string());
                trace(Trace::EffortNotAllowed {
                    turn,
                    body: self.reportable(body),
                });
                continue;
            }
            return Err(e);
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

/// What the answer to a check says of the model and effort.
fn checked(answer: genai::Result<(ChatResponse, Duration)>) -> Checked {
    match answer {
        // genai could not read a reply that came with a success status, which a reply cut off
        // after one token, such as in the middle of a tool call, need not be.
        Ok(_) | Err(genai::Error::ChatResponseGeneration { .. }) => Checked::Accepted,
        Err(e) => match refused(&e) {
            Some(message) => Checked::Refused(message),
            None => Checked::Unknown(why_failed(&e)),
        },
    }
}

/// The options of every request: the tools at the model's choice and the effort.
fn options(effort: &Effort) -> ChatOptions {
    // genai knows only a fixed list of efforts, and without one in the options it reads one from
    // the end of the model name, so that `qwen3-max` would go out as `qwen3` at effort `max`. A
    // budget stops that and adds nothing to an OpenAI request, so the effort goes in as given
    // through the extra body. For a model name it reads as GPT-5.6 or later, genai also adds
    // `prompt_cache_options`, which only OpenAI's own API knows, unless the options ask for a
    // cache. A five-minute one stops that and adds nothing to an OpenAI request either.
    let options = ChatOptions::default()
        .with_tool_choice(ToolChoice::Auto)
        .with_reasoning_effort(ReasoningEffort::Budget(0))
        .with_cache_control(CacheControl::Ephemeral5m);
    match effort.sent() {
        Some(effort) => options.with_extra_body(json!({ "reasoning_effort": effort })),
        None => options,
    }
}

/// The tools the model may call, the verdict last.
fn definitions() -> Vec<Tool> {
    let mut definitions = Tools::definitions();
    definitions.push(verdict_tool());
    definitions
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// `options` with the field that lets LiteLLM pass the effort on to a model it does not know to
/// take one.
fn with_effort_allowed(options: &ChatOptions) -> ChatOptions {
    let mut body = options.extra_body.clone().unwrap_or_else(|| json!({}));
    body["allowed_openai_params"] = json!(["reasoning_effort"]);
    options.clone().with_extra_body(body)
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
    use crate::testing::{
        EFFORT_NOT_ALLOWED, EFFORT_REFUSED, Reply, bad_request, failure, fake_model, one_token,
        too_many_requests, tool_call,
    };
    use crate::watcher::{Change, Filter};

    fn input<'a>(root: &'a std::path::Path, dmp: &'a str, changes: &'a [Change]) -> Input<'a> {
        Input {
            root,
            // Leaked, so that a test can build its input in the call that takes it.
            filter: Box::leak(Box::new(Filter::new(root).unwrap())),
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
                effort: "low".parse().unwrap(),
            },
            Some("sk-test".to_string()),
        )
        .unwrap()
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
        assert_eq!(body["allowed_openai_params"], Value::Null);
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
                Trace::EffortNotAllowed { turn, .. } => format!("request {turn} effort refused"),
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
    async fn sends_the_model_and_the_effort_by_the_names_given() {
        let root = tempfile::tempdir().unwrap();
        let mut bodies = Vec::new();
        for effort in ["default", "ultra"] {
            let (url, server) = fake_model(vec![verdict()]);
            let reviewer = Reviewer::new(
                &AiConfig {
                    url,
                    model: "qwen3-max".to_string(),
                    effort: effort.parse().unwrap(),
                },
                None,
            )
            .unwrap();
            reviewer
                .review(&input(root.path(), "{}", &[]), &|_| {})
                .await
                .unwrap();
            let mut body = server.join().unwrap().remove(0).1;
            body.as_object_mut().unwrap().retain(|key, _| {
                ["model", "reasoning_effort", "allowed_openai_params"].contains(&key.as_str())
            });
            bodies.push(body);
        }

        assert_eq!(
            bodies,
            [
                json!({ "model": "qwen3-max" }),
                json!({ "model": "qwen3-max", "reasoning_effort": "ultra" }),
            ]
        );
    }

    #[tokio::test]
    async fn sends_newer_gpt_models_only_the_fields_any_compatible_api_takes() {
        let root = tempfile::tempdir().unwrap();
        let (url, server) = fake_model(vec![verdict()]);
        let reviewer = Reviewer::new(
            &AiConfig {
                url,
                model: "gpt-5.6".to_string(),
                effort: Effort::default(),
            },
            None,
        )
        .unwrap();

        reviewer
            .review(&input(root.path(), "{}", &[]), &|_| {})
            .await
            .unwrap();

        let body = server.join().unwrap().remove(0).1;
        let mut fields: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            ["messages", "model", "stream", "tool_choice", "tools"]
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
    async fn sends_allowed_openai_params_to_a_model_once_the_api_asks_for_them() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, server) = fake_model(vec![
            bad_request(EFFORT_NOT_ALLOWED),
            verdict().into(),
            verdict().into(),
            verdict().into(),
            verdict().into(),
        ]);
        let reviewer = reviewer(endpoint);
        let changes = [Change::Created("log.txt".into())];

        let traces = Mutex::new(Vec::new());
        let verdict = reviewer
            .review(&input(root.path(), "{}", &changes), &|trace| {
                traces.lock().unwrap().push(trace)
            })
            .await
            .unwrap();
        reviewer
            .review(&input(root.path(), "{}", &changes), &|_| {})
            .await
            .unwrap();
        reviewer.set_choice(ModelChoice {
            model: "other".to_string(),
            effort: "low".parse().unwrap(),
        });
        reviewer
            .review(&input(root.path(), "{}", &changes), &|_| {})
            .await
            .unwrap();
        reviewer.set_choice(ModelChoice {
            model: "test".to_string(),
            effort: Effort::default(),
        });
        reviewer
            .review(&input(root.path(), "{}", &changes), &|_| {})
            .await
            .unwrap();

        assert_eq!(verdict.explanation, "A log file.");
        let requests = server.join().unwrap();
        let sent: Vec<(&Value, &Value, &Value)> = requests
            .iter()
            .map(|(_, body)| {
                (
                    &body["model"],
                    &body["reasoning_effort"],
                    &body["allowed_openai_params"],
                )
            })
            .collect();
        let (test, other, low) = (json!("test"), json!("other"), json!("low"));
        let allowed = json!(["reasoning_effort"]);
        assert_eq!(
            sent,
            [
                (&test, &low, &Value::Null),
                (&test, &low, &allowed),
                (&test, &low, &allowed),
                (&other, &low, &Value::Null),
                (&test, &Value::Null, &Value::Null),
            ]
        );
        let traces: Vec<Trace> = traces
            .into_inner()
            .unwrap()
            .into_iter()
            .filter(|trace| {
                matches!(
                    trace,
                    Trace::Request { .. } | Trace::EffortNotAllowed { .. }
                )
            })
            .collect();
        assert_eq!(
            traces,
            [
                Trace::Request { turn: 1 },
                Trace::EffortNotAllowed {
                    turn: 1,
                    body: json!({
                        "error": {
                            "message": EFFORT_NOT_ALLOWED,
                            "type": "invalid_request_error"
                        }
                    })
                    .to_string(),
                },
                Trace::Request { turn: 1 },
            ]
        );
    }

    #[tokio::test]
    async fn fails_when_the_api_asks_again_for_allowed_openai_params() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, _server) = fake_model(vec![
            bad_request(EFFORT_NOT_ALLOWED),
            bad_request(EFFORT_NOT_ALLOWED),
            verdict().into(),
        ]);

        let traces = Mutex::new(Vec::new());
        let e = reviewer(endpoint)
            .review(&input(root.path(), "{}", &[]), &|trace| {
                traces.lock().unwrap().push(trace)
            })
            .await
            .unwrap_err();

        assert_eq!(e.to_string(), "the model test failed");
        let traces = traces.into_inner().unwrap();
        let refused = traces
            .iter()
            .filter(|trace| matches!(trace, Trace::EffortNotAllowed { .. }))
            .count();
        assert_eq!(refused, 1);
        assert!(
            matches!(traces.last(), Some(Trace::Failed(e)) if e.contains("allowed_openai_params")),
            "{traces:?}"
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

    #[tokio::test]
    async fn fails_without_the_request_or_the_reply_when_the_reply_cannot_be_read() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("patients.csv"), "name\nAnna Muster\n").unwrap();
        // Tool calls from a small model: one whose arguments are not JSON, and one that writes
        // what it read where the function belongs.
        let mut cut_off = tool_call("read_file", json!({}));
        cut_off["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] =
            json!(r#"{"path": "notes"#);
        let mut misplaced = tool_call("read_file", json!({}));
        misplaced["choices"][0]["message"]["tool_calls"][0]["function"] = json!("Anna Muster");

        let mut failures = Vec::new();
        for unreadable in [cut_off, misplaced] {
            let (endpoint, _server) = fake_model(vec![
                tool_call("read_file", json!({ "path": "patients.csv" })),
                unreadable,
            ]);
            let traces = Mutex::new(Vec::new());
            let e = reviewer(endpoint)
                .review(
                    &input(
                        root.path(),
                        r#"{"dmp": {"title": "Consent study"}}"#,
                        &[Change::Created("patients.csv".into())],
                    ),
                    &|trace| traces.lock().unwrap().push(trace),
                )
                .await
                .unwrap_err();
            let Some(Trace::Failed(why)) = traces.into_inner().unwrap().pop() else {
                panic!("no failure traced last");
            };
            failures.push((format!("{e:#}"), why));
        }

        let why = [
            "the model's reply could not be read: Serde JSON error: EOF while parsing a string at \
             line 1 column 15",
            "the model's reply could not be read: Serde JSON error: invalid type: string \"…\", \
             expected struct IterimFunction",
        ];
        assert_eq!(
            failures,
            why.map(|why| (format!("the model test failed: {why}"), why.to_string()))
        );
    }

    #[tokio::test]
    async fn hides_the_api_key_and_cuts_what_the_api_answered_in_traces() {
        let root = tempfile::tempdir().unwrap();
        let limited = format!("Rate limit exceeded for sk-test. {}", "Wait. ".repeat(200));
        let not_allowed = format!("{EFFORT_NOT_ALLOWED} Key: sk-test");
        let (endpoint, _server) = fake_model(vec![
            too_many_requests(Some("0"), &limited),
            bad_request(&not_allowed),
            verdict().into(),
        ]);

        let traces = Mutex::new(Vec::new());
        reviewer(endpoint)
            .review(&input(root.path(), "{}", &[]), &|trace| {
                traces.lock().unwrap().push(trace)
            })
            .await
            .unwrap();

        let bodies: Vec<String> = traces
            .into_inner()
            .unwrap()
            .into_iter()
            .filter_map(|trace| match trace {
                Trace::RateLimited { body, .. } | Trace::EffortNotAllowed { body, .. } => {
                    Some(body)
                }
                _ => None,
            })
            .collect();
        let hidden = |message: &str, kind| {
            json!({ "error": { "message": message.replace("sk-test", "[API key]"), "type": kind } })
                .to_string()
        };
        let limited = hidden(&limited, "rate_limit_error");
        assert_eq!(
            bodies,
            [
                format!("{}…", &limited[..MAX_ANSWER]),
                hidden(&not_allowed, "invalid_request_error"),
            ]
        );
    }

    #[tokio::test]
    async fn checks_a_model_with_one_request_shaped_like_a_review() {
        let (url, server) = fake_model(vec![one_token()]);
        let reviewer = reviewer(url);

        let checked = reviewer.check(&reviewer.choice()).await;

        assert_eq!(checked, Checked::Accepted);
        let requests = server.join().unwrap();
        let body = &requests[0].1;
        assert_eq!(body["messages"][0]["content"], SYSTEM_PROMPT);
        let tools: Vec<&Value> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| &tool["function"]["name"])
            .collect();
        assert_eq!(tools, ["list_dir", "read_file", "report_verdict"]);
        assert_eq!(
            (
                &body["tool_choice"],
                &body["reasoning_effort"],
                &body["max_tokens"]
            ),
            (&json!("auto"), &json!("low"), &json!(1))
        );
    }

    #[tokio::test]
    async fn turns_down_an_effort_the_api_refuses_with_its_message() {
        let (url, server) = fake_model(vec![
            bad_request(EFFORT_NOT_ALLOWED),
            bad_request(EFFORT_REFUSED),
            bad_request(EFFORT_REFUSED),
        ]);
        let reviewer = reviewer(url);

        let checked = reviewer
            .check(&ModelChoice {
                model: "test".to_string(),
                effort: "high".parse().unwrap(),
            })
            .await;

        assert_eq!(checked, Checked::Refused(EFFORT_REFUSED.to_string()));
        let requests = server.join().unwrap();
        let sent: Vec<(&Value, &Value, &Value)> = requests
            .iter()
            .map(|(_, body)| {
                (
                    &body["reasoning_effort"],
                    &body["allowed_openai_params"],
                    &body["max_tokens"],
                )
            })
            .collect();
        let (high, allowed, one) = (json!("high"), json!(["reasoning_effort"]), json!(1));
        assert_eq!(
            sent,
            [
                (&high, &Value::Null, &one),
                (&high, &allowed, &one),
                (&high, &allowed, &Value::Null),
            ]
        );
    }

    #[tokio::test]
    async fn hides_the_api_key_in_what_a_check_reports() {
        // A message that quotes a body escaped as JSON, which may escape any character.
        let refused =
            r"Incorrect API key provided: sk-test, or {\u0022key\u0022: \u0022\u0073k-test\u0022}.";
        let (url, _server) = fake_model(vec![bad_request(refused), bad_request(refused)]);
        let reviewer = reviewer(url);

        let checked = reviewer.check(&reviewer.choice()).await;

        assert_eq!(
            checked,
            Checked::Refused(
                r"Incorrect API key provided: [API key], or {\u0022key\u0022: \u0022[API key]\u0022}."
                    .to_string()
            )
        );
    }

    #[tokio::test]
    async fn checks_again_without_the_token_limit_when_the_api_refuses_it() {
        let (url, server) = fake_model(vec![
            bad_request(
                "Unsupported parameter: 'max_tokens' is not supported with this model. Use \
                 'max_completion_tokens' instead.",
            ),
            one_token().into(),
        ]);
        let reviewer = reviewer(url);

        let checked = reviewer.check(&reviewer.choice()).await;

        assert_eq!(checked, Checked::Accepted);
        let requests = server.join().unwrap();
        let limits: Vec<&Value> = requests
            .iter()
            .map(|(_, body)| &body["max_tokens"])
            .collect();
        assert_eq!(limits, [&json!(1), &Value::Null]);
    }

    #[tokio::test]
    async fn cannot_tell_when_the_api_cannot_be_reached_is_busy_or_fails() {
        // A port that was just free, so the API cannot be reached.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let unreachable = reviewer(format!("http://127.0.0.1:{port}/v1"));
        let mut answers = vec![unreachable.check(&unreachable.choice()).await];
        for reply in [
            failure("408 Request Timeout", "The gateway timed out."),
            failure("409 Conflict", "No replica is free."),
            too_many_requests(None, "Rate limit exceeded."),
            failure("503 Service Unavailable", "The model is loading."),
        ] {
            let (url, _server) = fake_model(vec![reply]);
            let reviewer = reviewer(url);
            answers.push(reviewer.check(&reviewer.choice()).await);
        }

        assert!(
            matches!(&answers[0], Checked::Unknown(why) if why.starts_with(&format!(
                "could not reach the API: error sending request for url \
                 (http://127.0.0.1:{port}/v1/chat/completions)"
            ))),
            "{answers:?}"
        );
        // Another request would find no API, so these are the answers to the first.
        assert_eq!(
            answers[1..],
            [
                Checked::Unknown(
                    "the API answered 408 Request Timeout: The gateway timed out.".to_string()
                ),
                Checked::Unknown("the API answered 409 Conflict: No replica is free.".to_string()),
                Checked::Unknown(
                    "the API answered 429 Too Many Requests: Rate limit exceeded.".to_string()
                ),
                Checked::Unknown(
                    "the API answered 503 Service Unavailable: The model is loading.".to_string()
                ),
            ]
        );
    }
}
