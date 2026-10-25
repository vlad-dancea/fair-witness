//! What the runner prints about the runs once they are done.

use std::fmt::Write;

use crate::report::{Accuracy, Outcome};
use crate::run::effort_name;

/// Names the run of a scenario. `one_each` says that each scenario ran once.
pub fn label(outcome: &Outcome, fake: bool, one_each: bool) -> String {
    if fake && one_each {
        outcome.name.clone()
    } else if fake {
        format!("{}, run {}", outcome.name, outcome.repeat)
    } else {
        format!(
            "{}, {} {}, run {}",
            outcome.name,
            outcome.model,
            effort_name(outcome.effort),
            outcome.repeat
        )
    }
}

/// Which runs of the fake model passed. `one_each` says that each scenario ran once.
pub fn passed(outcomes: &[Outcome], one_each: bool) -> String {
    let labels: Vec<String> = outcomes
        .iter()
        .map(|outcome| label(outcome, true, one_each))
        .collect();
    let width = labels
        .iter()
        .map(String::len)
        .chain(["scenario".len()])
        .max()
        .unwrap_or_default();
    let mut text = format!("{:<6}{:<width$}  sets passed\n", "", "scenario");
    for (outcome, label) in outcomes.iter().zip(&labels) {
        let result = if outcome.passed { "pass" } else { "FAIL" };
        let _ = writeln!(
            text,
            "{result:<6}{label:<width$}  {} of {}",
            outcome.sets_passed(),
            outcome.sets.len()
        );
    }
    text
}

/// How often each real model and effort gave the expected verdict, overall and for each change
/// set, and why reviews failed.
pub fn accuracy(outcomes: &[Outcome], accuracy: &[Accuracy]) -> String {
    let mut rows = vec![
        [
            "model",
            "effort",
            "sets",
            "right",
            "wrong",
            "wrong field",
            "failed",
            "accuracy",
            "tokens in",
            "tokens out",
            "seconds",
        ]
        .map(str::to_string)
        .to_vec(),
    ];
    for row in accuracy {
        rows.push(vec![
            row.model.clone(),
            effort_name(row.effort).to_string(),
            row.sets.to_string(),
            row.correct.to_string(),
            row.wrong.to_string(),
            row.wrong_field.to_string(),
            row.failed.to_string(),
            format!("{:.0}%", row.accuracy * 100.0),
            row.tokens_in.to_string(),
            row.tokens_out.to_string(),
            format!("{:.1}", row.review_ms as f64 / 1000.0),
        ]);
    }
    let mut text = columns(&rows, 2);
    text.push_str("Tokens and seconds are means per review.\n\n");
    text.push_str(&sets(outcomes, accuracy));
    text.push_str(&failed_reviews(outcomes));
    text
}

/// A change set of a scenario, with the right verdicts and the runs of each model and effort.
struct SetRow<'a> {
    scenario: &'a str,
    number: usize,
    cells: Vec<(usize, usize)>,
}

/// How often each change set got the expected verdict from each model and effort, in the order
/// of `accuracy`.
fn sets(outcomes: &[Outcome], accuracy: &[Accuracy]) -> String {
    let mut sets: Vec<SetRow> = Vec::new();
    for outcome in outcomes {
        let Some(column) = accuracy
            .iter()
            .position(|row| row.model == outcome.model && row.effort == outcome.effort)
        else {
            continue;
        };
        for (number, set) in (1..).zip(&outcome.sets) {
            let row = sets
                .iter()
                .position(|row| row.scenario == outcome.name && row.number == number);
            let row = row.unwrap_or_else(|| {
                sets.push(SetRow {
                    scenario: &outcome.name,
                    number,
                    cells: vec![(0, 0); accuracy.len()],
                });
                sets.len() - 1
            });
            let (right, runs) = &mut sets[row].cells[column];
            *right += usize::from(set.correct == Some(true));
            *runs += 1;
        }
    }
    let mut header = vec!["scenario".to_string(), "set".to_string()];
    header.extend(
        accuracy
            .iter()
            .map(|row| format!("{} {}", row.model, effort_name(row.effort))),
    );
    let mut rows = vec![header];
    for set in sets {
        let mut row = vec![set.scenario.to_string(), set.number.to_string()];
        row.extend(
            set.cells
                .into_iter()
                .map(|(right, runs)| format!("{right} of {runs}")),
        );
        rows.push(row);
    }
    columns(&rows, 1)
}

/// Reviews of one model and effort that failed with errors alike but for their numbers.
struct Failed<'a> {
    model: String,
    shape: String,
    first: &'a str,
    times: usize,
    alike: bool,
}

/// Why reviews failed, once for each model, effort and kind of error, such as a wrong API key.
fn failed_reviews(outcomes: &[Outcome]) -> String {
    let mut failed: Vec<Failed> = Vec::new();
    for outcome in outcomes {
        let model = format!("{} {}", outcome.model, effort_name(outcome.effort));
        for error in outcome
            .sets
            .iter()
            .filter_map(|set| set.review_failed.as_deref())
        {
            let shape = without_numbers(error);
            match failed
                .iter_mut()
                .find(|kind| kind.model == model && kind.shape == shape)
            {
                Some(kind) => {
                    kind.times += 1;
                    kind.alike &= kind.first == error;
                }
                None => failed.push(Failed {
                    model: model.clone(),
                    shape,
                    first: error,
                    times: 1,
                    alike: true,
                }),
            }
        }
    }
    let mut text = String::new();
    if !failed.is_empty() {
        text.push_str("\nFailed reviews:\n");
    }
    for kind in failed {
        let times = match (kind.times, kind.alike) {
            (1, _) => "once".to_string(),
            (times, true) => format!("{times} times"),
            (times, false) => format!("{times} times, such as"),
        };
        let _ = writeln!(text, "  {}, {times}: {}", kind.model, indent(kind.first));
    }
    text
}

/// `error` with each run of digits replaced, so that errors that differ only in a number, such as
/// how long to wait before the next request, count as one.
fn without_numbers(error: &str) -> String {
    let mut shape = String::new();
    for c in error.chars() {
        if !c.is_ascii_digit() {
            shape.push(c);
        } else if !shape.ends_with('#') {
            shape.push('#');
        }
    }
    shape
}

/// What went wrong in each run that did not go as expected.
pub fn problems(outcomes: &[Outcome], fake: bool, one_each: bool) -> String {
    let mut text = String::new();
    for outcome in outcomes.iter().filter(|outcome| !outcome.passed) {
        let _ = writeln!(text, "\n{}:", label(outcome, fake, one_each));
        for problem in &outcome.problems {
            let _ = writeln!(text, "  {}", indent(problem));
        }
        for (number, set) in (1..).zip(&outcome.sets) {
            for problem in &set.problems {
                let _ = writeln!(text, "  set {number}: {}", indent(problem));
            }
        }
    }
    text
}

/// `rows` in columns, the first `left` of them aligned left and the others right.
fn columns(rows: &[Vec<String>], left: usize) -> String {
    let count = rows.iter().map(Vec::len).max().unwrap_or_default();
    let widths: Vec<usize> = (0..count)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(String::len)
                .max()
                .unwrap_or_default()
        })
        .collect();
    let mut text = String::new();
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .enumerate()
            .map(|(column, (cell, &width))| {
                if column < left {
                    format!("{cell:<width$}")
                } else {
                    format!("{cell:>width$}")
                }
            })
            .collect();
        let _ = writeln!(text, "{}", cells.join("  ").trim_end());
    }
    text
}

/// Indents the lines after the first of a problem, such as the output of a failed script, under
/// it.
fn indent(problem: &str) -> String {
    let lines: Vec<&str> = problem.trim_end().lines().map(str::trim_end).collect();
    lines.join("\n    ")
}

#[cfg(test)]
mod tests {
    use fair_witness_core::project::Effort;

    use super::*;
    use crate::fixtures::{expected, review, review_failed, scenario, verdict};
    use crate::report::{self, judge};
    use crate::run::{Model, Ran};

    #[test]
    fn shows_the_accuracy_of_each_model_and_of_each_change_set() {
        let notes = scenario("notes", vec![expected(None, false, None)]);
        let patients = scenario(
            "clinic-export",
            vec![
                expected(None, false, None),
                expected(None, true, Some("dataset[0].personal_data")),
            ],
        );
        let model = |name: &str, effort| Model::Real {
            url: "https://api.example.org/v1".to_string(),
            name: name.to_string(),
            effort,
        };
        let (large, small) = (model("large", Some(Effort::High)), model("small", None));
        let ran = |reviews: Vec<Vec<_>>| Ran {
            records: reviews.concat(),
            error: None,
        };
        let right = verdict(true, Some("dataset[0].personal_data"));
        let outcomes = [
            judge(
                &notes,
                &large,
                1,
                &ran(vec![review(1, 0, 3, (900, 40), verdict(true, None))]),
            ),
            judge(
                &patients,
                &large,
                1,
                &ran(vec![
                    review(1, 0, 2, (1000, 50), verdict(false, None)),
                    review(2, 2, 6, (1400, 70), right),
                ]),
            ),
            judge(
                &notes,
                &small,
                1,
                &ran(vec![review(
                    1,
                    0,
                    1,
                    (800, 10),
                    review_failed("429, retry after 3s"),
                )]),
            ),
            judge(
                &patients,
                &small,
                1,
                &ran(vec![
                    review(1, 0, 1, (800, 10), review_failed("429, retry after 12s")),
                    review(2, 1, 2, (800, 10), verdict(true, Some("dataset[0].title"))),
                ]),
            ),
        ];

        assert_eq!(
            accuracy(&outcomes, &report::accuracy(&outcomes)),
            "\
model  effort   sets  right  wrong  wrong field  failed  accuracy  tokens in  tokens out  seconds
large  high        3      2      1            0       0       67%       1100          53      3.0
small  default     3      0      0            1       2        0%        800          10      1.0
Tokens and seconds are means per review.

scenario       set  large high  small default
notes            1      0 of 1         0 of 1
clinic-export    1      1 of 1         0 of 1
clinic-export    2      1 of 1         0 of 1

Failed reviews:
  small default, 2 times, such as: 429, retry after 3s
"
        );
    }
}
