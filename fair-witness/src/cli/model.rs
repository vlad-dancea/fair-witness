use anyhow::{Context, Result, bail};
use clap::Args;
use dialoguer::{FuzzySelect, Input, Password};
use reqwest::blocking::Client;

use super::connect::{http_client, web_url};
use crate::agent;
use crate::config::{AiConfig, Config, Credentials};

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

pub fn setup(args: ModelArgs) -> Result<()> {
    let mut config = Config::load()?.context("this folder is not set up yet")?;
    let http = http_client()?;
    let previous = config.ai.take();
    let saved_key = Credentials::load()?.api_key;
    let (url, api_key, models) = match args.ai_url {
        Some(url) => {
            let url = web_url(&url)?;
            let api_key = args.api_key.or(saved_key);
            let models = agent::list_models(&http, &url, api_key.as_deref())?;
            (url, api_key, models)
        }
        None => ask_for_api(
            &http,
            previous.as_ref().map(|ai| ai.url.clone()),
            args.api_key,
            saved_key,
        )?,
    };
    let effort = previous.as_ref().and_then(|ai| ai.effort);
    let model = choose_model(&url, models, args.model, previous.map(|ai| ai.model))?;

    let mut credentials = Credentials::load()?;
    credentials.api_key = api_key;
    credentials.save()?;
    println!("Changes will be checked with {model}.");
    config.ai = Some(AiConfig { url, model, effort });
    config.save()?;
    Ok(())
}

fn ask_for_api(
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
        match agent::list_models(http, &url, api_key.as_deref()) {
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
