use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// A model of an API and how much it reasons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    pub model: String,
    #[serde(default, skip_serializing_if = "Effort::is_default")]
    pub effort: Effort,
}

impl fmt::Display for ModelChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.effort.sent() {
            Some(effort) => write!(f, "{} ({effort} effort)", self.model),
            None => f.write_str(&self.model),
        }
    }
}

/// How much a model reasons: a name the API gets as `reasoning_effort`, or `default`, which sends
/// none so that the model uses its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Effort(String);

const DEFAULT_EFFORT: &str = "default";
/// The efforts the model panel offers. `none` asks for no reasoning, which some models ignore.
const OFFERED_EFFORTS: [&str; 8] = [
    DEFAULT_EFFORT,
    "none",
    "minimal",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
];

impl Effort {
    pub fn offered() -> Vec<Effort> {
        OFFERED_EFFORTS
            .iter()
            .map(|name| Effort(name.to_string()))
            .collect()
    }

    pub fn name(&self) -> &str {
        &self.0
    }

    pub fn is_default(&self) -> bool {
        self.0 == DEFAULT_EFFORT
    }

    /// What goes to the API as `reasoning_effort`, nothing for `default`.
    pub fn sent(&self) -> Option<&str> {
        (!self.is_default()).then_some(&self.0)
    }
}

impl Default for Effort {
    fn default() -> Self {
        Effort(DEFAULT_EFFORT.to_string())
    }
}

impl FromStr for Effort {
    type Err = anyhow::Error;

    // No standard fixes the names, and each model takes its own with its own meaning, so any
    // name passes on as given.
    fn from_str(name: &str) -> Result<Self> {
        if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            bail!(
                "{name:?} is not a reasoning effort; name one such as `low` or `high`, or \
                 `default` to send none"
            );
        }
        Ok(Effort(name.to_string()))
    }
}

impl TryFrom<String> for Effort {
    type Error = anyhow::Error;

    fn try_from(name: String) -> Result<Self> {
        name.parse()
    }
}

impl From<Effort> for String {
    fn from(effort: Effort) -> Self {
        effort.0
    }
}
