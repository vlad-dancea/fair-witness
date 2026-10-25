//! Scenarios and events for the tests.

use std::path::PathBuf;

use fair_witness_core::session::Record;
use fair_witness_core::watcher::Change;
use serde_json::{Value, json};

use crate::scenario::{Expected, Scenario};

pub fn scenario(name: &str, sets: Vec<Expected>) -> Scenario {
    Scenario {
        name: name.to_string(),
        dir: PathBuf::new(),
        description: String::new(),
        sets,
    }
}

pub fn expected(changes: Option<Vec<Change>>, contradicts: bool, field: Option<&str>) -> Expected {
    Expected {
        changes,
        contradicts,
        dmp_field: field.into_iter().map(str::to_string).collect(),
        explanation: "Why.".to_string(),
        fake_tool_calls: Vec::new(),
    }
}

pub fn record(event: Value) -> Record {
    record_at(0, event)
}

pub fn record_at(second: u32, event: Value) -> Record {
    let mut record = event;
    record["time"] = json!(format!("2026-10-24T10:00:{second:02}Z"));
    serde_json::from_value(record).unwrap()
}

/// The events of a review of change set `set` from second `start` to `end`, which used `tokens`
/// and ended with `last`.
pub fn review(set: usize, start: u32, end: u32, tokens: (u32, u32), last: Value) -> Vec<Record> {
    let mut last = last;
    last["set"] = json!(set);
    vec![
        record_at(
            start,
            json!({ "event": "changes", "set": set, "changes": [
                { "created": format!("file-{set}.csv") }
            ] }),
        ),
        record_at(start, json!({ "event": "reviewing", "set": set })),
        record_at(
            end,
            json!({ "event": "trace", "set": set, "trace": { "reply": {
                "turn": 1, "tokens_in": tokens.0, "tokens_out": tokens.1, "elapsed_ms": 1
            } } }),
        ),
        record_at(end, last),
    ]
}

pub fn verdict(contradicts: bool, field: Option<&str>) -> Value {
    json!({ "event": "verdict", "verdict": {
        "contradicts": contradicts, "dmp_field": field, "explanation": "Because."
    } })
}

pub fn review_failed(error: &str) -> Value {
    json!({ "event": "review_failed", "error": error })
}
