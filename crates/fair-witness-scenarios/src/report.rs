use fair_witness_core::escape;
use fair_witness_core::model::Effort;
use fair_witness_core::review::{Trace, Verdict};
use fair_witness_core::session::{Event, Record};
use fair_witness_core::watcher::Change;
use jiff::Timestamp;
use serde::Serialize;

use crate::run::{Model, Ran};
use crate::scenario::{Expected, Scenario};

/// A model and effort that the API refused when the runner checked them, so that they did not
/// run.
#[derive(Debug, PartialEq, Serialize)]
pub struct Refused {
    pub model: String,
    #[serde(skip_serializing_if = "Effort::is_default")]
    pub effort: Effort,
    /// What the API said.
    pub error: String,
}

/// How a run of a scenario went.
#[derive(Debug, Serialize)]
pub struct RunOutcome {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub model: String,
    #[serde(skip_serializing_if = "Effort::is_default")]
    pub effort: Effort,
    /// Which run of the scenario with this model and effort it is, from 1.
    pub repeat: usize,
    /// The file with the run's events, relative to `results.json`.
    pub events: String,
    /// Whether the run went as the scenario expects. With a real model, a wrong verdict or a
    /// failed review does not fail it, since that is what the run measures.
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
    /// Whether the verdict is the expected one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correct: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_failed: Option<String>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// From the start of the review to its verdict or failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_ms: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    #[serde(skip)]
    reviewing: Option<Timestamp>,
    /// The tool calls that failed, with why.
    #[serde(skip)]
    failed_calls: Vec<String>,
}

/// How often a model at one effort gave the expected verdict, and what a review took.
#[derive(Debug, PartialEq, Serialize)]
pub struct Accuracy {
    pub model: String,
    #[serde(skip_serializing_if = "Effort::is_default")]
    pub effort: Effort,
    pub sets: usize,
    pub correct: usize,
    /// Sets whose verdict says the opposite of what the scenario expects.
    pub wrong: usize,
    /// Sets whose verdict finds the expected contradiction but names a field that `dmp_field`
    /// does not accept.
    pub wrong_field: usize,
    /// Sets without a verdict, since the review failed or never ended.
    pub failed: usize,
    /// The share of the sets with the expected verdict.
    pub accuracy: f64,
    /// The means of the reviews that ended, with a verdict or a failure.
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub review_ms: u64,
}

impl RunOutcome {
    pub fn sets_passed(&self) -> usize {
        self.sets.iter().filter(|set| set.passed()).count()
    }

    pub fn sets_correct(&self) -> usize {
        self.sets
            .iter()
            .filter(|set| set.correct == Some(true))
            .count()
    }
}

impl SetOutcome {
    fn new(expected: Expected) -> Self {
        Self {
            expected,
            changes: None,
            verdict: None,
            correct: None,
            review_failed: None,
            tokens_in: 0,
            tokens_out: 0,
            review_ms: None,
            problems: Vec::new(),
            reviewing: None,
            failed_calls: Vec::new(),
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
                self.review_ms = self.since_reviewing(record.time);
            }
            Event::ReviewFailed { error, .. } => {
                self.review_failed = Some(error.clone());
                self.review_ms = self.since_reviewing(record.time);
            }
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
                self.failed_calls
                    .push(format!("the tool call {name} failed: {error}"));
            }
            _ => {}
        }
    }

    fn since_reviewing(&self, time: Timestamp) -> Option<i64> {
        self.reviewing.map(|start| {
            let elapsed = time.duration_since(start).as_millis();
            i64::try_from(elapsed).unwrap_or(i64::MAX)
        })
    }

    /// Lists how the set differs from what was expected. A run that stopped early has already
    /// said why, so a `finished` one alone reports the sets it never got to. The fake model's
    /// tool calls and verdict are the scenario's own, so with it a failed call, a failed review
    /// or another verdict is a mistake in the scenario or a broken tool. A real model's are what
    /// the run measures.
    fn compare(&mut self, finished: bool, fake: bool) {
        if fake {
            self.problems.append(&mut self.failed_calls);
        }
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
        if fake && let Some(error) = &self.review_failed {
            self.problems.push(format!("the review failed: {error}"));
        }
        let Some(verdict) = &self.verdict else {
            if finished && !decided && self.changes.is_some() {
                self.problems.push("no verdict".to_string());
            }
            return;
        };
        let correct = expected.accepts(verdict);
        self.correct = Some(correct);
        if fake && !correct {
            self.problems.push(format!(
                "expected {}, but the verdict is {}: {:?}",
                describe(expected.contradicts, &expected.dmp_field),
                describe(verdict.contradicts, verdict.dmp_field.as_slice()),
                verdict.explanation
            ));
        }
    }
}

/// Compares the events of run `repeat` of a scenario with `model` with what the scenario
/// expects of each change set.
pub fn judge(scenario: &Scenario, model: &Model, repeat: usize, ran: &Ran) -> RunOutcome {
    let fake = *model == Model::Fake;
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
            Event::WatchStopped { error } => {
                problems.push(format!("watching stopped: {error}"));
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
        set.compare(ran.error.is_none(), fake);
    }
    RunOutcome {
        name: scenario.name.clone(),
        description: scenario.description.clone(),
        model: model.name().to_string(),
        effort: model.effort(),
        repeat,
        events: format!("{}/{}-{repeat}.jsonl", model.folder(), scenario.name),
        passed: problems.is_empty() && sets.iter().all(|set| set.problems.is_empty()),
        problems,
        sets,
    }
}

/// The accuracy of each model and effort in `outcomes`, in the order they first come.
pub fn accuracy(outcomes: &[RunOutcome]) -> Vec<Accuracy> {
    let mut groups: Vec<(&str, &Effort, Vec<&SetOutcome>)> = Vec::new();
    for outcome in outcomes {
        let group = groups
            .iter()
            .position(|(model, effort, _)| *model == outcome.model && **effort == outcome.effort);
        let group = group.unwrap_or_else(|| {
            groups.push((&outcome.model, &outcome.effort, Vec::new()));
            groups.len() - 1
        });
        groups[group].2.extend(&outcome.sets);
    }
    groups
        .into_iter()
        .map(|(model, effort, sets)| {
            let count = |counts: fn(&SetOutcome, &Verdict) -> bool| {
                sets.iter()
                    .filter(|set| {
                        set.verdict
                            .as_ref()
                            .is_some_and(|verdict| counts(set, verdict))
                    })
                    .count()
            };
            let correct = count(|set, _| set.correct == Some(true));
            let wrong = count(|set, verdict| verdict.contradicts != set.expected.contradicts);
            let wrong_field = count(|set, verdict| {
                set.correct == Some(false) && verdict.contradicts == set.expected.contradicts
            });
            let reviewed: Vec<&SetOutcome> = sets
                .iter()
                .copied()
                .filter(|set| set.verdict.is_some() || set.review_failed.is_some())
                .collect();
            Accuracy {
                model: model.to_string(),
                effort: effort.clone(),
                sets: sets.len(),
                correct,
                wrong,
                wrong_field,
                failed: sets.len() - correct - wrong - wrong_field,
                accuracy: if sets.is_empty() {
                    0.0
                } else {
                    correct as f64 / sets.len() as f64
                },
                tokens_in: mean(reviewed.iter().map(|set| set.tokens_in)),
                tokens_out: mean(reviewed.iter().map(|set| set.tokens_out)),
                review_ms: mean(
                    sets.iter()
                        .filter_map(|set| set.review_ms)
                        .map(|ms| u64::try_from(ms).unwrap_or(0)),
                ),
            }
        })
        .collect()
}

/// The mean of `values`, rounded, or 0 without any.
fn mean(values: impl Iterator<Item = u64>) -> u64 {
    let (sum, count) = values.fold((0, 0), |(sum, count), value| (sum + value, count + 1));
    (sum + count / 2).checked_div(count).unwrap_or(0)
}

/// Compares the changes themselves, in any order. Their descriptions can read alike, since
/// "renamed a to b to c" can be either of two renames.
fn same_changes(want: &[Change], got: &[Change]) -> bool {
    fn sorted(changes: &[Change]) -> Vec<&Change> {
        let mut sorted: Vec<&Change> = changes.iter().collect();
        sorted.sort_by_key(|&change| match change {
            Change::Renamed { from, to } => (change.kind(), from.as_path(), Some(to.as_path())),
            _ => (change.kind(), change.path(), None),
        });
        sorted
    }
    sorted(want) == sorted(got)
}

fn describe_changes(changes: &[Change]) -> String {
    let described: Vec<String> = changes.iter().map(describe_change).collect();
    format!("[{}]", described.join(", "))
}

fn describe_change(change: &Change) -> String {
    let verb = change.kind().verb();
    match change {
        Change::Renamed { from, to } => {
            format!("{verb} {} to {}", escape::path(from), escape::path(to))
        }
        _ => format!("{verb} {}", escape::path(change.path())),
    }
}

fn describe(contradicts: bool, fields: &[String]) -> String {
    match (contradicts, fields) {
        (false, _) => "no contradiction".to_string(),
        (true, []) => "a contradiction".to_string(),
        (true, fields) => format!("a contradiction in {}", fields.join(" or ")),
    }
}

#[cfg(test)]
mod tests {
    use fair_witness_core::model::ModelChoice;
    use fair_witness_core::project::AiConfig;
    use serde_json::json;

    use super::*;
    use crate::fixtures::{expected, record, review, review_failed, scenario, verdict};

    #[test]
    fn lists_what_differs_from_each_expected_change_set() {
        let scenario = scenario(
            "patients",
            vec![
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
        );
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

        let outcome = judge(&scenario, &Model::Fake, 1, &ran);

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

    #[test]
    fn tells_apart_renames_that_read_alike() {
        let renamed = |from: &str, to: &str| Change::Renamed {
            from: from.into(),
            to: to.into(),
        };
        let want = [renamed("a", "b to c"), Change::Created("d".into())];

        // Both read "renamed a to b to c".
        assert!(!same_changes(
            &want,
            &[renamed("a to b", "c"), Change::Created("d".into())]
        ));
        assert!(same_changes(
            &want,
            &[Change::Created("d".into()), renamed("a", "b to c")]
        ));
    }

    #[test]
    fn measures_a_real_model_instead_of_failing_on_its_verdicts() {
        let scenario = scenario(
            "patients",
            vec![
                expected(None, false, None),
                expected(None, true, Some("dataset[0].personal_data")),
                expected(None, true, None),
            ],
        );
        let model = Model::Real(AiConfig {
            url: "https://api.example.org/v1".to_string(),
            choice: ModelChoice {
                model: "openai/large".to_string(),
                effort: "high".parse().unwrap(),
            },
        });
        let mut records = review(1, 0, 2, (100, 10), verdict(false, None));
        records.extend(review(
            2,
            2,
            6,
            (300, 30),
            verdict(true, Some("dataset[0].sensitive_data")),
        ));
        // A real model may ask for a file that is not there.
        records.insert(
            records.len() - 1,
            record(
                json!({ "event": "trace", "set": 2, "trace": { "tool_result": {
                "name": "read_file", "output": "error: not found", "failed": true
            } } }),
            ),
        );
        records.extend(review(
            3,
            6,
            7,
            (50, 5),
            review_failed("429 Too Many Requests"),
        ));
        let first = judge(
            &scenario,
            &model,
            1,
            &Ran {
                records,
                error: None,
            },
        );
        let mut records = review(1, 0, 1, (100, 10), verdict(false, None));
        records.extend(review(
            2,
            1,
            2,
            (100, 10),
            verdict(true, Some("dmp.dataset[0].personal_data")),
        ));
        records.extend(review(
            3,
            2,
            3,
            (100, 10),
            verdict(true, Some("dataset[0].title")),
        ));
        let second = judge(
            &scenario,
            &model,
            2,
            &Ran {
                records,
                error: None,
            },
        );

        assert!(first.passed && second.passed, "{first:?}\n{second:?}");
        assert_eq!(first.events, "openai_large-high/patients-1.jsonl");
        let correct: Vec<Option<bool>> = first.sets.iter().map(|set| set.correct).collect();
        assert_eq!(correct, [Some(true), Some(false), None]);
        assert_eq!(
            accuracy(&[first, second]),
            [Accuracy {
                model: "openai/large".to_string(),
                effort: "high".parse().unwrap(),
                sets: 6,
                correct: 4,
                wrong: 0,
                wrong_field: 1,
                failed: 1,
                accuracy: 4.0 / 6.0,
                // (100 + 300 + 50 + 3 * 100) / 6 and (10 + 30 + 5 + 3 * 10) / 6, rounded
                tokens_in: 125,
                tokens_out: 13,
                // (2 + 4 + 1 + 3 * 1) seconds / 6
                review_ms: 1667,
            }]
        );
    }
}
