mod agent;
mod prompt;
mod provider;
mod steps;
mod tools;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::ModelChoice;

pub use agent::{ChoiceCheck, ModelStep};
pub use prompt::first_message;
pub use provider::list_models;
pub(crate) use provider::retry_after;
pub use steps::{Input, Step, StepFuture, StepOutcome, run_steps};

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
        /// What the API answered, as `ModelStep::reportable` leaves it.
        body: String,
    },
    /// The API refused request `turn` and asked to name its effort in `allowed_openai_params`,
    /// as LiteLLM does, so it goes again with the field, which later requests to the model
    /// carry from the start.
    EffortNotAllowed {
        turn: usize,
        /// What the API answered, as `ModelStep::reportable` leaves it.
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
    /// Why a request or the whole review failed, in one line as `ModelStep::reportable` leaves
    /// it. A request whose reply could not be read goes again after it.
    Failed(String),
}

fn is_false(value: &bool) -> bool {
    !value
}
