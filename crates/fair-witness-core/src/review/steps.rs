use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Result, bail};

use super::{Trace, Verdict};
use crate::watcher::{Change, Filter};

/// What a step of a review looks at.
pub struct Input<'a> {
    pub root: &'a Path,
    /// What in `root` is not research data, which the watcher leaves out and no step may read.
    pub filter: &'a Filter,
    pub dmp: &'a str,
    pub changes: &'a [Change],
    /// What earlier steps found, for the later ones to take into account.
    pub found: &'a [String],
}

/// What a step makes of a change set.
#[derive(Debug)]
pub enum Outcome {
    Decided(Verdict),
    /// Hands the change set on to the next step, with what this one found, if anything.
    Undecided(Option<String>),
}

pub type StepFuture<'a> = Pin<Box<dyn Future<Output = Result<Outcome>> + Send + 'a>>;

/// One step of a review. A review runs its steps in order until one decides, and the model
/// is the last.
pub trait Step: Send + Sync {
    fn check<'a>(
        &'a self,
        input: &'a Input<'a>,
        trace: &'a (dyn Fn(Trace) + Sync),
    ) -> StepFuture<'a>;
}

/// Runs `steps` over `changes` until one decides.
pub async fn review(
    steps: &[Arc<dyn Step>],
    root: &Path,
    filter: &Filter,
    dmp: &str,
    changes: &[Change],
    trace: &(dyn Fn(Trace) + Sync),
) -> Result<Verdict> {
    let mut found = Vec::new();
    for step in steps {
        let input = Input {
            root,
            filter,
            dmp,
            changes,
            found: &found,
        };
        match step.check(&input, trace).await? {
            Outcome::Decided(verdict) => return Ok(verdict),
            Outcome::Undecided(Some(finding)) => found.push(finding),
            Outcome::Undecided(None) => {}
        }
    }
    bail!("no step of the review reached a verdict")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::project::{AiConfig, Effort};
    use crate::review::ModelStep;
    use crate::testing::{Reply, fake_model, tool_call};

    /// A step that gives the same outcome for every change set.
    struct Fixed(fn() -> Outcome);

    impl Step for Fixed {
        fn check<'a>(&'a self, _: &'a Input<'a>, _: &'a (dyn Fn(Trace) + Sync)) -> StepFuture<'a> {
            Box::pin(async { Ok((self.0)()) })
        }
    }

    fn model(url: String) -> Arc<dyn Step> {
        let config = AiConfig {
            url,
            model: "test".to_string(),
            effort: Effort::default(),
        };
        Arc::new(ModelStep::new(&config, None).unwrap())
    }

    fn verdict() -> Verdict {
        Verdict {
            contradicts: true,
            dmp_field: Some("dataset[0].personal_data".to_string()),
            explanation: "Patient names, but the DMP says no personal data.".to_string(),
        }
    }

    #[tokio::test]
    async fn stops_at_the_first_step_that_decides() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, server) = fake_model(Vec::<Reply>::new());
        let steps = [
            Arc::new(Fixed(|| Outcome::Decided(verdict()))),
            model(endpoint),
        ];

        let filter = Filter::new(root.path()).unwrap();
        let decided = review(&steps, root.path(), &filter, "{}", &[], &|_| {}).await;

        assert_eq!(decided.unwrap(), verdict());
        assert!(server.join().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tells_the_model_what_earlier_steps_found() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, server) = fake_model(vec![tool_call(
            "report_verdict",
            json!({ "contradicts": false, "explanation": "Survey answers, as planned." }),
        )]);
        let steps = [
            Arc::new(Fixed(|| Outcome::Undecided(None))),
            Arc::new(Fixed(|| {
                Outcome::Undecided(Some("survey.csv has a column of names.".to_string()))
            })),
            model(endpoint),
        ];

        std::fs::write(root.path().join("survey.csv"), "name\nAnna Muster\n").unwrap();
        let changes = [Change::Created("survey.csv".into())];
        let filter = Filter::new(root.path()).unwrap();
        review(&steps, root.path(), &filter, "{}", &changes, &|_| {})
            .await
            .unwrap();

        let body = &server.join().unwrap()[0].1;
        let prompt = body["messages"][1]["content"].as_str().unwrap();
        assert!(
            prompt.ends_with(
                "What changed in the project folder:\n\
                 created: survey.csv\n\n\
                 Earlier checks found:\n\
                 - survey.csv has a column of names."
            ),
            "{prompt}"
        );
    }
}
