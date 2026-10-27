use std::sync::Arc;

use anyhow::Result;
use fair_witness_core::model::ModelChoice;
use fair_witness_core::project::Project;
use fair_witness_core::review::ModelStep;

/// The model `run` reviews with, which the model panel can change while it watches.
pub struct ModelSettings {
    pub project: Project,
    pub reviewer: Arc<ModelStep>,
    pub url: String,
    pub api_key: Option<String>,
}

impl ModelSettings {
    /// Reviews with `choice` from the next review on, and saves it for the next `run`, which
    /// can fail after the choice is in use.
    pub fn apply(&self, choice: ModelChoice) -> Result<()> {
        self.reviewer.set_choice(choice.clone());
        self.project.save_model_choice(&self.url, &choice)
    }
}

#[cfg(test)]
mod tests {
    use fair_witness_core::model::Effort;
    use fair_witness_core::project::{AiConfig, Config, DamapConfig};

    use super::*;

    #[test]
    fn uses_a_choice_it_cannot_save_until_run_ends() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        let small = ModelChoice {
            model: "small".to_string(),
            effort: Effort::default(),
        };
        project
            .save_config(&Config {
                damap: DamapConfig {
                    frontend_url: "https://damap.example.org".to_string(),
                    dmp_id: None,
                },
                ai: Some(AiConfig {
                    url: "https://api.example.org/v1".to_string(),
                    choice: small.clone(),
                }),
            })
            .unwrap();
        // As with --ai-url.
        let other = AiConfig {
            url: "http://localhost:11434/v1".to_string(),
            choice: small,
        };
        let settings = ModelSettings {
            project: project.clone(),
            reviewer: Arc::new(ModelStep::new(&other, None).unwrap()),
            url: other.url.clone(),
            api_key: None,
        };
        let large = ModelChoice {
            model: "large".to_string(),
            effort: "high".parse().unwrap(),
        };

        let saved = settings.apply(large.clone());

        assert_eq!(
            saved.unwrap_err().to_string(),
            "http://localhost:11434/v1 is not the API chosen in `fair-witness setup`"
        );
        assert_eq!(settings.reviewer.choice(), large);
        assert_eq!(
            project.config().unwrap().unwrap().ai.unwrap().choice.model,
            "small"
        );
    }
}
