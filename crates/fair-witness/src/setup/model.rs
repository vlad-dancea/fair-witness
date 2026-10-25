use anyhow::{Context, Result, bail};
use clap::Args;
use dialoguer::{FuzzySelect, Input, Password};
use fair_witness_core::project::{AiConfig, Project};
use fair_witness_core::review;
use reqwest::Client;

use super::damap::{http_client, web_url};

const DEFAULT_URL: &str = "https://aqueduct.ai.datalab.tuwien.ac.at/v1";

#[derive(Args)]
pub struct ModelArgs {
    #[arg(
        long,
        env = "FAIR_WITNESS_AI_URL",
        help = "Base URL of an OpenAI-compatible API for the model; asked for if not given"
    )]
    pub ai_url: Option<String>,
    #[arg(
        long,
        env = "FAIR_WITNESS_API_KEY",
        hide_env_values = true,
        help = "API key for it, if it needs one; asked for along with the URL"
    )]
    pub api_key: Option<String>,
    #[arg(
        long,
        env = "FAIR_WITNESS_MODEL",
        help = "The model to use; chosen from the ones the API offers if not given"
    )]
    pub model: Option<String>,
}

pub async fn setup(project: &Project, args: ModelArgs) -> Result<()> {
    let mut config = project.config()?.context("this folder is not set up yet")?;
    let http = http_client()?;
    let previous = config.ai.take();
    let saved_key = project.credentials()?.api_key;
    let (url, api_key, models) = match args.ai_url {
        Some(url) => {
            let url = web_url(&url)?;
            let api_key = args.api_key.or(saved_key);
            let models = review::list_models(&http, &url, api_key.as_deref()).await?;
            (url, api_key, models)
        }
        None => {
            ask_for_api(
                &http,
                previous.as_ref().map(|ai| ai.url.clone()),
                args.api_key,
                saved_key,
            )
            .await?
        }
    };
    let model = choose_model(
        &url,
        models,
        args.model,
        previous.as_ref().map(|ai| ai.model.clone()),
    )?;
    let (ai, line) = chosen(previous, url, model);

    let mut credentials = project.credentials()?;
    credentials.api_key = api_key;
    project.save_credentials(&credentials)?;
    println!("{line}");
    config.ai = Some(ai);
    project.save_config(&config)
}

/// The settings to save for `model` and the line that names it. The saved effort stays only
/// with the API and model it was chosen for, since an effort one model takes can fail every
/// review on another.
fn chosen(previous: Option<AiConfig>, url: String, model: String) -> (AiConfig, String) {
    let (effort, line) = match previous.and_then(|ai| Some((ai.effort?, ai))) {
        Some((effort, ai)) if ai.url == url && ai.model == model => (
            Some(effort),
            format!(
                "Changes will be checked with {model} ({} effort).",
                effort.name()
            ),
        ),
        Some((effort, ai)) => {
            let was_for = if ai.model == model {
                format!("{} at {}", ai.model, ai.url)
            } else {
                ai.model
            };
            let line = format!(
                "Changes will be checked with {model} (default effort), since the {} effort \
                 saved before was for {was_for}. Press `m` in `fair-witness run` to choose one.",
                effort.name()
            );
            (None, line)
        }
        None => (None, format!("Changes will be checked with {model}.")),
    };
    (AiConfig { url, model, effort }, line)
}

async fn ask_for_api(
    http: &Client,
    previous: Option<String>,
    given_key: Option<String>,
    saved_key: Option<String>,
) -> Result<(String, Option<String>, Vec<String>)> {
    let mut default = previous.unwrap_or_else(|| DEFAULT_URL.to_string());
    loop {
        let input: String = Input::new()
            .with_prompt("AI API URL (OpenAI-compatible)")
            .default(default.clone())
            .interact_text()?;
        let url = match web_url(&input) {
            Ok(url) => url,
            Err(e) => {
                println!("{e}. Try again, or press Ctrl+C to stop.");
                continue;
            }
        };
        let api_key = match &given_key {
            Some(key) => Some(key.clone()),
            None => {
                let prompt = match saved_key {
                    Some(_) => "API key (empty keeps the saved one)",
                    None => "API key (empty for none)",
                };
                let key = Password::new()
                    .with_prompt(prompt)
                    .allow_empty_password(true)
                    .interact()?;
                Some(key)
                    .filter(|key| !key.is_empty())
                    .or_else(|| saved_key.clone())
            }
        };
        match review::list_models(http, &url, api_key.as_deref()).await {
            Ok(models) => return Ok((url, api_key, models)),
            Err(e) => println!("{e:#}. Try again, or press Ctrl+C to stop."),
        }
        default = url;
    }
}

fn choose_model(
    url: &str,
    models: Vec<String>,
    given: Option<String>,
    previous: Option<String>,
) -> Result<String> {
    if let Some(model) = given {
        if !models.contains(&model) {
            println!("Note: {url} does not list {model}; using it anyway.");
        }
        return Ok(model);
    }
    if models.is_empty() {
        bail!("{url} offers no models; install one, or pass --model");
    }
    let default = previous
        .and_then(|previous| models.iter().position(|model| *model == previous))
        .unwrap_or(0);
    let index = FuzzySelect::new()
        .with_prompt("Model (type to filter)")
        .items(&models)
        .default(default)
        .interact()?;
    Ok(models[index].clone())
}

#[cfg(test)]
mod tests {
    use fair_witness_core::project::Effort;

    use super::*;

    #[test]
    fn keeps_the_saved_effort_only_for_the_same_model() {
        let saved = |effort| {
            Some(AiConfig {
                url: "https://api.example.org/v1".to_string(),
                model: "small".to_string(),
                effort,
            })
        };
        let outcome = |previous, url: &str, model: &str| {
            let (ai, line) = chosen(previous, url.to_string(), model.to_string());
            (ai.effort, line)
        };

        assert_eq!(
            [
                outcome(
                    saved(Some(Effort::High)),
                    "https://api.example.org/v1",
                    "small"
                ),
                outcome(
                    saved(Some(Effort::High)),
                    "https://api.example.org/v1",
                    "large"
                ),
                // The same name on another API can be another model, or refuse the effort.
                outcome(
                    saved(Some(Effort::Low)),
                    "https://other.example.org/v1",
                    "small"
                ),
                outcome(saved(None), "https://api.example.org/v1", "large"),
            ],
            [
                (
                    Some(Effort::High),
                    "Changes will be checked with small (high effort).".to_string()
                ),
                (
                    None,
                    "Changes will be checked with large (default effort), since the high effort \
                     saved before was for small. Press `m` in `fair-witness run` to choose one."
                        .to_string()
                ),
                (
                    None,
                    "Changes will be checked with small (default effort), since the low effort \
                     saved before was for small at https://api.example.org/v1. Press `m` in \
                     `fair-witness run` to choose one."
                        .to_string()
                ),
                (None, "Changes will be checked with large.".to_string()),
            ]
        );
    }
}
