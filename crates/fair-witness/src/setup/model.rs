use anyhow::{Context, Result, bail};
use clap::Args;
use dialoguer::{FuzzySelect, Input, Password};
use fair_witness_core::net::{api_key_unencrypted, in_the_clear, same_url, web_url};
use fair_witness_core::project::{AiConfig, Effort, Project};
use fair_witness_core::review::{self, ChoiceCheck, ModelChoice, ModelStep};

use crate::tui::model_panel::describe;

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
        help = "API key for it, if it needs one, or empty to remove the saved one; asked for \
                along with the URL"
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
    let mut previous = config.ai.take();
    let mut credentials = project.credentials()?;
    // An empty --api-key removes the saved key before the API is asked anything, since an API
    // that needs a key refuses to answer without one and `setup` would stop with the key saved.
    if args.api_key.as_deref() == Some("") && credentials.api_key.take().is_some() {
        project.save_credentials(&credentials)?;
        println!("Removed the saved API key.");
    }
    let saved_key = SavedKey {
        url: previous.as_ref().map(|ai| ai.url.clone()),
        key: credentials.api_key,
    };
    let (url, api_key, models) = match args.ai_url {
        Some(url) => {
            let url = web_url(&url)?;
            let api_key = match args.api_key {
                Some(key) if key.is_empty() => None,
                Some(_) if in_the_clear(&url) => {
                    bail!(
                        "{}, or leave out --api-key if it needs none",
                        api_key_unencrypted(&url)
                    )
                }
                Some(key) => Some(key),
                None if in_the_clear(&url) => {
                    println!("{}", no_key_sent(&url));
                    None
                }
                None => saved_key.for_api(&url),
            };
            let models = review::list_models(&url, api_key.as_deref()).await?;
            (url, api_key, models)
        }
        None => {
            ask_for_api(
                previous.as_ref().map(|ai| ai.url.clone()),
                args.api_key,
                saved_key,
            )
            .await?
        }
    };
    let mut model = choose_model(
        &url,
        &models,
        args.model.clone(),
        previous.as_ref().map(|ai| ai.model.clone()),
    )?;
    // The model and effort are checked last, so that one the API refuses is chosen again.
    let (ai, line) = loop {
        let (ai, line) = chosen(previous.clone(), url.clone(), model.clone());
        let choice = ModelChoice {
            model: ai.model.clone(),
            effort: ai.effort.clone(),
        };
        let described = describe(&choice);
        match ModelStep::new(&ai, api_key.clone())?
            .try_choice(&choice)
            .await
        {
            ChoiceCheck::Accepted => break (ai, line),
            ChoiceCheck::Unknown(why) => {
                println!(
                    "Could not check {described}, so the reviews will show whether the API takes \
                     it: {why}"
                );
                break (ai, line);
            }
            // `setup` asks for no effort, so the one kept from before goes, and the model is
            // checked at its own.
            ChoiceCheck::Refused(message) if !ai.effort.is_default() => {
                println!("{url} refuses {described}: {message}");
                previous = None;
            }
            ChoiceCheck::Refused(message) if args.model.is_some() => bail!(
                "{url} refuses {described}: {message}\nChoose another with --model, or leave it \
                 out to choose one from the list"
            ),
            ChoiceCheck::Refused(message) => {
                println!("{url} refuses {described}: {message}");
                println!("Choose another model, or press Ctrl+C to stop.");
                model = choose_model(&url, &models, None, Some(model))?;
            }
        }
    };

    let mut credentials = project.credentials()?;
    credentials.api_key = api_key;
    project.save_credentials(&credentials)?;
    println!("{line}");
    config.ai = Some(ai);
    project.save_config(&config)
}

/// The API key in `credentials.toml` and the API in `config.toml` it was saved for.
struct SavedKey {
    url: Option<String>,
    key: Option<String>,
}

impl SavedKey {
    /// The saved key, if it was saved for `url`, since another API would get a key that is not
    /// its own.
    fn for_api(&self, url: &str) -> Option<String> {
        self.key.clone().filter(|_| {
            self.url
                .as_deref()
                .is_some_and(|saved| same_url(saved, url))
        })
    }
}

/// The settings to save for `model` and the line that names it. The saved effort stays only
/// with the API and model it was chosen for, since an effort one model takes can fail every
/// review on another.
fn chosen(previous: Option<AiConfig>, url: String, model: String) -> (AiConfig, String) {
    let (effort, line) = match previous.filter(|ai| !ai.effort.is_default()) {
        Some(ai) if same_url(&ai.url, &url) && ai.model == model => {
            let line = format!(
                "Changes will be checked with {model} ({} effort).",
                ai.effort.name()
            );
            (ai.effort, line)
        }
        Some(ai) => {
            let was_for = if ai.model == model {
                format!("{} at {}", ai.model, ai.url)
            } else {
                ai.model
            };
            let line = format!(
                "Changes will be checked with {model} (default effort), since the {} effort \
                 saved before was for {was_for}. Press `m` in `fair-witness run` to choose one.",
                ai.effort.name()
            );
            (Effort::default(), line)
        }
        None => (
            Effort::default(),
            format!("Changes will be checked with {model}."),
        ),
    };
    (AiConfig { url, model, effort }, line)
}

async fn ask_for_api(
    previous: Option<String>,
    given_key: Option<String>,
    saved_key: SavedKey,
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
            Some(key) if key.is_empty() => None,
            Some(_) if in_the_clear(&url) => {
                println!(
                    "{}, or leave out --api-key if it needs none. Try again, or press Ctrl+C to \
                     stop.",
                    api_key_unencrypted(&url)
                );
                default = url;
                continue;
            }
            Some(key) => Some(key.clone()),
            None if in_the_clear(&url) => {
                println!("{}", no_key_sent(&url));
                None
            }
            None => {
                let saved_key = saved_key.for_api(&url);
                let prompt = match saved_key {
                    Some(_) => "API key (empty keeps the saved one)",
                    None => "API key (empty for none)",
                };
                let key = Password::new()
                    .with_prompt(prompt)
                    .allow_empty_password(true)
                    .interact()?;
                Some(key).filter(|key| !key.is_empty()).or(saved_key)
            }
        };
        match review::list_models(&url, api_key.as_deref()).await {
            Ok(models) => return Ok((url, api_key, models)),
            Err(e) => println!("{e:#}. Try again, or press Ctrl+C to stop."),
        }
        default = url;
    }
}

/// Why `setup` neither asks for a key for `url` nor sends the saved one, which may be for
/// another API.
fn no_key_sent(url: &str) -> String {
    format!("No API key is sent to {url}, since it uses plain HTTP to another machine.")
}

fn choose_model(
    url: &str,
    models: &[String],
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
        .items(models)
        .default(default)
        .interact()?;
    Ok(models[index].clone())
}

#[cfg(test)]
mod tests {
    use fair_witness_core::project::{Config, Credentials, DamapConfig};
    use fair_witness_core::testing::{
        EFFORT_REFUSED, Reply, Request, bad_request, failure, fake_model, one_token,
    };
    use serde_json::{Value, json};

    use super::*;

    /// A folder set up with DAMAP, with `ai` and `api_key` saved.
    fn folder(ai: Option<AiConfig>, api_key: Option<&str>) -> (tempfile::TempDir, Project) {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        project
            .save_config(&Config {
                damap: DamapConfig {
                    frontend_url: "https://damap.example.org".to_string(),
                    dmp_id: Some(3),
                },
                ai,
            })
            .unwrap();
        project
            .save_credentials(&Credentials {
                api_key: api_key.map(str::to_string),
                ..Credentials::default()
            })
            .unwrap();
        (root, project)
    }

    fn models() -> Reply {
        json!({ "object": "list", "data": [{ "id": "small", "object": "model" }] }).into()
    }

    /// Sets up the model of a folder whose config holds `saved`, with `model` given and an API
    /// that lists `small` and then answers with `checks`. Gives back the AI settings it saved,
    /// or why it failed, and the requests the API got.
    async fn set_up(
        saved: Option<(&str, &str)>,
        model: &str,
        checks: Vec<Reply>,
    ) -> (Result<Option<AiConfig>, String>, String, Vec<Request>) {
        let mut replies = vec![models()];
        replies.extend(checks);
        let (url, server) = fake_model(replies);
        let ai = saved.map(|(model, effort)| AiConfig {
            url: url.clone(),
            model: model.to_string(),
            effort: effort.parse().unwrap(),
        });
        let (_root, project) = folder(ai, None);

        let args = ModelArgs {
            ai_url: Some(url.clone()),
            api_key: None,
            model: Some(model.to_string()),
        };
        let set_up = setup(&project, args)
            .await
            .map(|()| project.config().unwrap().unwrap().ai)
            .map_err(|e| e.to_string());
        (set_up, url, server.join().unwrap())
    }

    #[tokio::test]
    async fn drops_a_saved_effort_the_api_refuses() {
        let (set_up, _, requests) = set_up(
            Some(("small", "high")),
            "small",
            vec![
                bad_request(EFFORT_REFUSED),
                bad_request(EFFORT_REFUSED),
                one_token().into(),
            ],
        )
        .await;

        let ai = set_up.unwrap().unwrap();
        assert_eq!((ai.model.as_str(), ai.effort), ("small", Effort::default()));
        let efforts: Vec<&Value> = requests[1..]
            .iter()
            .map(|(_, body)| &body["reasoning_effort"])
            .collect();
        assert_eq!(efforts, [&json!("high"), &json!("high"), &Value::Null]);
    }

    #[tokio::test]
    async fn fails_when_the_api_refuses_the_model_given() {
        let refused = "The model `gone` does not exist.";
        let (set_up, url, _) = set_up(
            Some(("small", "default")),
            "gone",
            vec![bad_request(refused), bad_request(refused)],
        )
        .await;

        assert_eq!(
            set_up.unwrap_err(),
            format!(
                "{url} refuses gone: {refused}\nChoose another with --model, or leave it out to \
                 choose one from the list"
            )
        );
    }

    #[tokio::test]
    async fn sends_the_saved_api_key_only_to_its_api() {
        let (url, server) = fake_model(vec![
            models(),
            one_token().into(),
            models(),
            one_token().into(),
            failure("401 Unauthorized", "No API key given."),
        ]);
        let args = |api_key: Option<&str>| ModelArgs {
            ai_url: Some(url.clone()),
            api_key: api_key.map(str::to_string),
            model: Some("small".to_string()),
        };
        let key_saved_for = |url: &str| {
            let ai = AiConfig {
                url: url.to_string(),
                model: "small".to_string(),
                effort: Effort::default(),
            };
            folder(Some(ai), Some("sk-saved"))
        };
        let saved_key = |project: &Project| project.credentials().unwrap().api_key;
        let (_other, another_api) = key_saved_for("https://other.example.org/v1");
        let (_same, same_api) = key_saved_for(&url);

        setup(&another_api, args(None)).await.unwrap();
        setup(&same_api, args(None)).await.unwrap();
        let kept = saved_key(&same_api);
        // An empty --api-key removes the saved key, also when the API then wants one.
        assert_eq!(
            setup(&same_api, args(Some("")))
                .await
                .unwrap_err()
                .to_string(),
            format!("{url}/models answered 401 Unauthorized; is the URL right, and the API key?")
        );

        let requests = server.join().unwrap();
        assert_eq!(
            [saved_key(&another_api), kept, saved_key(&same_api)],
            [None, Some("sk-saved".to_string()), None]
        );
        let authorization: Vec<Option<&str>> = requests
            .iter()
            .map(|(head, _)| {
                head.lines()
                    .find_map(|line| line.strip_prefix("authorization: "))
            })
            .collect();
        let saved = Some("Bearer sk-saved");
        assert_eq!(authorization, [None, None, saved, saved, None]);
    }

    #[tokio::test]
    async fn sends_no_api_key_over_plain_http_to_another_machine() {
        let (_root, project) = folder(None, Some("sk-saved"));
        let (url, server) = fake_model(vec![models(), one_token().into()]);
        // 0.0.0.0 reaches this machine on Linux and macOS, but fair-witness cannot tell that it
        // is not another one.
        let url = url.replace("127.0.0.1", "0.0.0.0");
        let args = |api_key: Option<&str>| ModelArgs {
            ai_url: Some(url.clone()),
            api_key: api_key.map(str::to_string),
            model: Some("small".to_string()),
        };

        assert_eq!(
            setup(&project, args(Some("sk-given")))
                .await
                .unwrap_err()
                .to_string(),
            format!(
                "{url} uses plain HTTP to another machine, which would send the API key \
                 unencrypted\nUse https://, or reach the API through this machine, such as at \
                 http://localhost through an SSH tunnel, or leave out --api-key if it needs none"
            )
        );
        setup(&project, args(None)).await.unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|(head, _)| !head.contains("sk-")));
        assert_eq!(project.credentials().unwrap().api_key, None);
    }

    #[test]
    fn keeps_the_saved_effort_only_for_the_same_model() {
        let saved = |effort: &str| {
            Some(AiConfig {
                url: "https://api.example.org/v1".to_string(),
                model: "small".to_string(),
                effort: effort.parse().unwrap(),
            })
        };
        let outcome = |previous, url: &str, model: &str| {
            let (ai, line) = chosen(previous, url.to_string(), model.to_string());
            (ai.effort, line)
        };

        assert_eq!(
            [
                outcome(saved("high"), "https://api.example.org/v1", "small"),
                outcome(saved("high"), "https://api.example.org/v1", "large"),
                // The same name on another API can be another model, or refuse the effort.
                outcome(saved("low"), "https://other.example.org/v1", "small"),
                outcome(saved("default"), "https://api.example.org/v1", "large"),
            ],
            [
                (
                    "high".parse().unwrap(),
                    "Changes will be checked with small (high effort).".to_string()
                ),
                (
                    Effort::default(),
                    "Changes will be checked with large (default effort), since the high effort \
                     saved before was for small. Press `m` in `fair-witness run` to choose one."
                        .to_string()
                ),
                (
                    Effort::default(),
                    "Changes will be checked with small (default effort), since the low effort \
                     saved before was for small at https://api.example.org/v1. Press `m` in \
                     `fair-witness run` to choose one."
                        .to_string()
                ),
                (
                    Effort::default(),
                    "Changes will be checked with large.".to_string()
                ),
            ]
        );
    }
}
