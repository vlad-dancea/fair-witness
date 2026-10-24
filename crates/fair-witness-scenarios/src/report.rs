use fair_witness_core::review::{Trace, Verdict};
use fair_witness_core::session::{Event, Record};
use fair_witness_core::watcher::Change;
use jiff::Timestamp;
use serde::Serialize;

use crate::run::Ran;
use crate::scenario::{Expected, Scenario};

/// How a scenario went.
#[derive(Debug, Serialize)]
pub struct Outcome {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub passed: bool,
    /// What went wrong outside any one change set, such as a script that failed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    pub sets: Vec<SetOutcome>,
}

/// How one change set went.
#[derive(Debug, Serialize)]
pub struct SetOutcome {
    pub expected: Expected,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<Vec<Change>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_failed: Option<String>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// From the start of the review to its verdict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_ms: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    #[serde(skip)]
    reviewing: Option<Timestamp>,
}

impl Outcome {
    /// A scenario that could not be read.
    pub fn invalid(name: String, error: String) -> Self {
        Self {
            name,
            description: String::new(),
            passed: false,
            problems: vec![error],
            sets: Vec::new(),
        }
    }

    pub fn sets_passed(&self) -> usize {
        self.sets.iter().filter(|set| set.passed()).count()
    }
}

impl SetOutcome {
    fn new(expected: Expected) -> Self {
        Self {
            expected,
            changes: None,
            verdict: None,
            review_failed: None,
            tokens_in: 0,
            tokens_out: 0,
            review_ms: None,
            problems: Vec::new(),
            reviewing: None,
        }
    }

    pub fn passed(&self) -> bool {
        self.problems.is_empty() && self.verdict.is_some()
    }

    fn add(&mut self, record: &Record) {
        match &record.event {
            Event::Changes { changes, .. } => self.changes = Some(changes.clone()),
            Event::Reviewing { .. } => self.reviewing = Some(record.time),
            Event::Trace {
                trace:
                    Trace::Reply {
                        tokens_in,
                        tokens_out,
                        ..
                    },
                ..
            } => {
                self.tokens_in += tokens_in.and_then(|n| u64::try_from(n).ok()).unwrap_or(0);
                self.tokens_out += tokens_out.and_then(|n| u64::try_from(n).ok()).unwrap_or(0);
            }
            Event::Verdict { verdict, .. } => {
                self.verdict = Some(verdict.clone());
                self.review_ms = self.reviewing.map(|start| {
                    let elapsed = record.time.duration_since(start).as_millis();
                    i64::try_from(elapsed).unwrap_or(i64::MAX)
                });
            }
            Event::ReviewFailed { error, .. } => self.review_failed = Some(error.clone()),
            // The fake model's calls are the scenario's own, so one that fails is a mistake in
            // the scenario or a broken tool.
            Event::Trace {
                trace:
                    Trace::ToolResult {
                        name,
                        output,
                        failed: true,
                    },
                ..
            } => {
                let error = output.strip_prefix("error: ").unwrap_or(output);
                self.problems
                    .push(format!("the tool call {name} failed: {error}"));
            }
            _ => {}
        }
    }

    /// Lists how the set differs from what was expected. A run that stopped early has already
    /// said why, so a `finished` one alone reports the sets it never got to.
    fn compare(&mut self, finished: bool) {
        let expected = &self.expected;
        let decided = self.verdict.is_some() || self.review_failed.is_some();
        match (&expected.changes, &self.changes) {
            (Some(want), Some(got)) if !same_changes(want, got) => {
                self.problems.push(format!(
                    "expected the changes {}, but saw {}",
                    describe_changes(want),
                    describe_changes(got)
                ));
            }
            (_, None) if finished || decided => self.problems.push("no change set".to_string()),
            _ => {}
        }
        if let Some(error) = &self.review_failed {
            self.problems.push(format!("the review failed: {error}"));
        }
        let Some(verdict) = &self.verdict else {
            if finished && !decided && self.changes.is_some() {
                self.problems.push("no verdict".to_string());
            }
            return;
        };
        let field_differs = expected.dmp_field.is_some() && verdict.dmp_field != expected.dmp_field;
        if verdict.contradicts != expected.contradicts || (verdict.contradicts && field_differs) {
            self.problems.push(format!(
                "expected {}, but the verdict is {}: {:?}",
                describe(expected.contradicts, expected.dmp_field.as_deref()),
                describe(verdict.contradicts, verdict.dmp_field.as_deref()),
                verdict.explanation
            ));
        }
    }
}

/// Compares the events of a run with what the scenario expects of each change set.
pub fn judge(scenario: &Scenario, ran: &Ran) -> Outcome {
    let mut problems: Vec<String> = ran.error.iter().cloned().collect();
    let mut sets: Vec<SetOutcome> = scenario.sets.iter().cloned().map(SetOutcome::new).collect();
    for record in &ran.records {
        let set = match &record.event {
            Event::Changes { set, .. }
            | Event::Reviewing { set }
            | Event::Trace { set, .. }
            | Event::Verdict { set, .. }
            | Event::ReviewFailed { set, .. } => *set,
            Event::WatchErrors { errors, .. } => {
                problems.push(format!("watching failed: {}", errors.join("; ")));
                continue;
            }
            // Only a folder that is set up gets a log.
            Event::LogStopped { .. } => continue,
        };
        match set.checked_sub(1).and_then(|index| sets.get_mut(index)) {
            Some(outcome) => outcome.add(record),
            None => {
                if let Event::Changes { changes, .. } = &record.event {
                    problems.push(format!(
                        "change set {set} was not expected: {}",
                        describe_changes(changes)
                    ));
                }
            }
        }
    }
    for set in &mut sets {
        set.compare(ran.error.is_none());
    }
    Outcome {
        name: scenario.name.clone(),
        description: scenario.description.clone(),
        passed: problems.is_empty() && sets.iter().all(SetOutcome::passed),
        problems,
        sets,
    }
}

fn same_changes(want: &[Change], got: &[Change]) -> bool {
    let sorted = |changes: &[Change]| {
        let mut described: Vec<String> = changes.iter().map(describe_change).collect();
        described.sort();
        described
    };
    sorted(want) == sorted(got)
}

fn describe_changes(changes: &[Change]) -> String {
    let described: Vec<String> = changes.iter().map(describe_change).collect();
    format!("[{}]", described.join(", "))
}

fn describe_change(change: &Change) -> String {
    let verb = change.kind().verb();
    match change {
        Change::Renamed { from, to } => format!("{verb} {} to {}", from.display(), to.display()),
        _ => format!("{verb} {}", change.path().display()),
    }
}

fn describe(contradicts: bool, field: Option<&str>) -> String {
    match (contradicts, field) {
        (false, _) => "no contradiction".to_string(),
        (true, Some(field)) => format!("a contradiction in {field}"),
        (true, None) => "a contradiction".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn record(event: serde_json::Value) -> Record {
        let mut record = event;
        record["time"] = json!("2026-10-24T10:00:00Z");
        serde_json::from_value(record).unwrap()
    }

    fn expected(changes: Option<Vec<Change>>, contradicts: bool, field: Option<&str>) -> Expected {
        Expected {
            changes,
            contradicts,
            dmp_field: field.map(str::to_string),
            explanation: "Why.".to_string(),
            fake_tool_calls: Vec::new(),
        }
    }

    #[test]
    fn lists_what_differs_from_each_expected_change_set() {
        let scenario = Scenario {
            name: "patients".to_string(),
            dir: PathBuf::new(),
            description: String::new(),
            sets: vec![
                expected(
                    Some(vec![
                        Change::Created("notes".into()),
                        Change::Created("notes/consent.md".into()),
                    ]),
                    false,
                    None,
                ),
                expected(None, true, Some("dataset[0].personal_data")),
                expected(None, true, Some("dataset[0].personal_data")),
                expected(Some(vec![Change::Created("notes.txt".into())]), false, None),
            ],
        };
        let ran = Ran {
            records: vec![
                // The same changes as expected, in another order.
                record(json!({ "event": "changes", "set": 1, "changes": [
                    { "created": "notes/consent.md" }, { "created": "notes" }
                ] })),
                record(json!({ "event": "verdict", "set": 1, "verdict": {
                    "contradicts": false, "explanation": "Notes."
                } })),
                record(json!({ "event": "changes", "set": 2, "changes": [
                    { "created": "patients.csv" }
                ] })),
                record(json!({ "event": "verdict", "set": 2, "verdict": {
                    "contradicts": true,
                    "dmp_field": "dataset[0].sensitive_data",
                    "explanation": "Diagnoses."
                } })),
                record(json!({ "event": "changes", "set": 3, "changes": [
                    { "renamed": { "from": "patients.csv", "to": "people.csv" } }
                ] })),
                record(json!({ "event": "review_failed", "set": 3, "error": "timed out" })),
                // A verdict without its change set, after a file that reads like an error and a
                // tool call that failed.
                record(
                    json!({ "event": "trace", "set": 4, "trace": { "tool_result": {
                    "name": "read_file", "output": "error: disk full"
                } } }),
                ),
                record(
                    json!({ "event": "trace", "set": 4, "trace": { "tool_result": {
                    "name": "read_flie", "output": "error: unknown tool `read_flie`", "failed": true
                } } }),
                ),
                record(json!({ "event": "verdict", "set": 4, "verdict": {
                    "contradicts": false, "explanation": "Notes."
                } })),
                record(json!({ "event": "changes", "set": 5, "changes": [
                    { "deleted": "people.csv" }
                ] })),
            ],
            error: None,
        };

        let outcome = judge(&scenario, &ran);

        assert!(!outcome.passed);
        assert_eq!(outcome.sets_passed(), 1);
        assert_eq!(
            outcome.problems,
            ["change set 5 was not expected: [deleted people.csv]"]
        );
        let problems: Vec<&[String]> = outcome
            .sets
            .iter()
            .map(|set| set.problems.as_slice())
            .collect();
        assert_eq!(
            problems,
            [
                &[][..],
                &[
                    "expected a contradiction in dataset[0].personal_data, but the verdict is a \
                     contradiction in dataset[0].sensitive_data: \"Diagnoses.\""
                        .to_string()
                ],
                &["the review failed: timed out".to_string()],
                &[
                    "the tool call read_flie failed: unknown tool `read_flie`".to_string(),
                    "no change set".to_string()
                ],
            ]
        );
    }
}
