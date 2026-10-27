use std::env::{self, VarError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use fair_witness_core::auth::Prompts;
use fair_witness_core::damap::{self, Damap};
use fair_witness_core::net::{api_key_unencrypted, in_the_clear, same_url, web_url};
use fair_witness_core::project::{AiConfig, Config, Effort, NOT_SET_UP, Project};
use fair_witness_core::review::{Checked, Reviewer};
use fair_witness_core::session::{Record, Session};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use super::Failure;
use crate::setup::damap::TerminalPrompts;
use crate::tui::app::{Screen, Update};
use crate::tui::model_panel::{self, ModelSettings};
use crate::{diagnostics, notifications};

const DEBOUNCE_VARIABLE: &str = "FAIR_WITNESS_DEBOUNCE_MS";
const STOP_WRITING_WITHIN: Duration = Duration::from_secs(2);

#[derive(clap::Args)]
pub struct Args {
    #[arg(
        long,
        conflicts_with = "dmp_file",
        help = "Check changes against the DMP with this id instead of the one chosen in `setup`"
    )]
    dmp: Option<i64>,
    #[arg(
        long,
        value_name = "PATH",
        help = "Check changes against the maDMP JSON in this file, without DAMAP"
    )]
    dmp_file: Option<PathBuf>,
    #[arg(
        long,
        value_name = "DIR",
        help = "Run as if started in this folder instead of the current one"
    )]
    project: Option<PathBuf>,
    #[arg(
        long,
        help = "Print each event as a line of JSON instead of drawing the screen, until stdin closes"
    )]
    headless: bool,
    #[arg(
        long,
        env = "FAIR_WITNESS_AI_URL",
        help = "Base URL of an OpenAI-compatible API, instead of the one chosen in `setup`"
    )]
    ai_url: Option<String>,
    // So that a key exported for one API never goes to the API a folder was set up with.
    #[arg(
        long,
        env = "FAIR_WITNESS_API_KEY",
        hide_env_values = true,
        requires = "ai_url",
        help = "API key for the API at --ai-url, if it needs one"
    )]
    api_key: Option<String>,
    #[arg(
        long,
        env = "FAIR_WITNESS_MODEL",
        help = "The model to use, instead of the one chosen in `setup`"
    )]
    model: Option<String>,
    #[arg(
        long,
        env = "FAIR_WITNESS_EFFORT",
        help = "How much the model reasons, by a name it takes such as low or xhigh, instead of \
                the saved effort. `default` sends no effort, so the model uses its own, and \
                `none` asks for no reasoning, which some models ignore"
    )]
    effort: Option<Effort>,
    #[arg(
        long,
        hide = true,
        help = "Do not check the model and effort before watching, for a caller that checked them \
                already, such as the scenario runner"
    )]
    skip_model_check: bool,
}

/// What `run` watches with, once it has everything it needs.
struct Start {
    project: Project,
    records: UnboundedReceiver<Record>,
    status: Vec<String>,
    settings: Option<ModelSettings>,
}

enum Source {
    File(PathBuf),
    Damap(i64),
}

/// The DMP to check changes against, and the header line that names it.
struct Dmp {
    json: String,
    status: String,
}

pub async fn run(here: &Path, args: Args) -> Result<(), Failure> {
    if !args.headless {
        let start = start(here, args).await.map_err(Failure::Setup)?;
        return show_events(start).await.map_err(Failure::Runtime);
    }
    // Listening before startup, which can wait on DAMAP for a while, lets a script stop it then.
    let stopped = stop_requested().map_err(Failure::Setup)?;
    tokio::pin!(stopped);
    let start = tokio::select! {
        start = start(here, args) => start.map_err(Failure::Setup)?,
        () = &mut stopped => return Ok(()),
    };
    print_events(start, stopped).await.map_err(Failure::Runtime)
}

async fn start(here: &Path, args: Args) -> Result<Start> {
    let dir = match &args.project {
        // Resolved, so that `..` leads where the shell would and not to the current project.
        Some(dir) => here
            .join(dir)
            .canonicalize()
            .with_context(|| format!("could not find {}", dir.display()))?,
        None => here.to_path_buf(),
    };
    if !dir.is_dir() {
        bail!("{} is not a folder", dir.display());
    }
    let project = Project::find(&dir).unwrap_or_else(|| Project::new(dir));
    let config = project.config()?;
    if config.is_none() && args.dmp_file.is_none() {
        bail!(NOT_SET_UP);
    }
    let debounce = debounce()?;
    let mut status = Vec::new();
    let ai = ai(&project, config.as_ref(), &args, &mut status)?;
    let saved_dmp = config.as_ref().and_then(|config| config.damap.dmp_id);

    let mut session = Session::new(project.clone());
    // A folder that never ran `setup` gets no log, since `run` writes nothing into it.
    if config.is_some() {
        let log = project.run_log()?;
        diagnostics::write_to(&log.diagnostics)?;
        session = session.logging_to(log.events);
    }
    tracing::info!("watching {}", project.root().display());
    if let Some(debounce) = debounce {
        session = session.debounce(debounce);
    }
    let mut settings = None;
    let dmp_given = args.dmp_file.is_some() || args.dmp.is_some();
    let source = match (args.dmp_file, args.dmp.or(saved_dmp)) {
        (Some(file), _) => Some(Source::File(file)),
        (None, id) => id.map(Source::Damap),
    };
    match (source, ai) {
        (Some(source), Some((ai, api_key))) => {
            let reviewer = Arc::new(Reviewer::new(&ai, api_key.clone())?);
            // Before the DMP, so that a refusal comes before any DAMAP login.
            if !args.skip_model_check {
                check_model(&reviewer, &ai.url, &mut status).await?;
            }
            let dmp = match source {
                Source::File(file) => read_dmp(&file)?,
                Source::Damap(id) => download_dmp(&project, id, args.headless, &mut status).await?,
            };
            session = session.reviewing(dmp.json, vec![reviewer.clone()]);
            tracing::info!("{}", dmp.status);
            status.insert(0, dmp.status);
            settings = Some(ModelSettings {
                project: project.clone(),
                reviewer,
                url: ai.url,
                api_key,
            });
        }
        (Some(_), None) if dmp_given => bail!(
            "no AI model to check with; choose one with `fair-witness setup`, \
             or pass --ai-url and --model"
        ),
        (None, Some(_)) => status.push(
            "No DMP chosen, so changes are not checked; choose one with `fair-witness setup`."
                .to_string(),
        ),
        _ => status.push("Changes are not checked.".to_string()),
    }

    let records = session.start()?;
    Ok(Start {
        project,
        records,
        status,
        settings,
    })
}

/// Asks the API once whether it takes the model and effort, so that a choice it refuses stops
/// `run` at once instead of failing every review. When the check cannot tell, `run` still starts,
/// with a warning in `status`.
async fn check_model(reviewer: &Reviewer, url: &str, status: &mut Vec<String>) -> Result<()> {
    let choice = reviewer.choice();
    let described = model_panel::describe(&choice);
    match reviewer.check(&choice).await {
        Checked::Accepted => Ok(()),
        Checked::Refused(message) => bail!(
            "{url} refuses {described}: {message}\nPass another model with --model or another \
             effort with --effort, or choose a model with `fair-witness setup`"
        ),
        Checked::Unknown(why) => {
            let warning = format!(
                "Warning: could not check {described}, so the reviews will show whether the API \
                 takes it: {why}"
            );
            tracing::warn!("{warning}");
            status.extend(warning.lines().map(str::to_string));
            Ok(())
        }
    }
}

fn debounce() -> Result<Option<Duration>> {
    match env::var(DEBOUNCE_VARIABLE) {
        Ok(ms) => match ms.trim().parse() {
            Ok(ms) => Ok(Some(Duration::from_millis(ms))),
            Err(_) => bail!("{DEBOUNCE_VARIABLE} must be a number of milliseconds, not {ms:?}"),
        },
        Err(VarError::NotPresent) => Ok(None),
        Err(e) => Err(e).context(DEBOUNCE_VARIABLE),
    }
}

/// The model to check with and its API key, from the flags and what `setup` saved. Why the saved
/// key is not sent, when it is for another API than the saved one, goes into `status`.
fn ai(
    project: &Project,
    config: Option<&Config>,
    args: &Args,
    status: &mut Vec<String>,
) -> Result<Option<(AiConfig, Option<String>)>> {
    let saved = config.and_then(|config| config.ai.clone());
    let url = match &args.ai_url {
        Some(url) => web_url(url)?,
        None => match &saved {
            Some(ai) => ai.url.clone(),
            None if args.model.is_some() => bail!("--model needs --ai-url too"),
            None => return Ok(None),
        },
    };
    // Only the API from `setup` gets its saved model and effort.
    let saved_api = saved.filter(|ai| same_url(&ai.url, &url));
    let model = match (&args.model, &saved_api) {
        (Some(model), _) => model.clone(),
        (None, Some(ai)) => ai.model.clone(),
        (None, None) => bail!("no model chosen for {url}; pass --model"),
    };
    let effort = match &args.effort {
        Some(effort) => effort.clone(),
        None => saved_api
            .as_ref()
            .filter(|ai| ai.model == model)
            .map(|ai| ai.effort.clone())
            .unwrap_or_default(),
    };
    let api_key = match (&args.api_key, project.credentials()?.ai) {
        (Some(key), _) => Some(key.clone()),
        (None, Some(saved)) => match saved.for_api(&url) {
            Some(key) => Some(key.to_string()),
            // As after an edit of config.toml, or a `setup` that saved the key but not the API
            // it is for.
            None if saved_api.is_some() => {
                status.push(format!(
                    "The saved API key is for {}, so none is sent to {url}. To save one for \
                     {url}, run `fair-witness setup`.",
                    saved.url
                ));
                None
            }
            None => None,
        },
        (None, None) => None,
    };
    // Also for an API saved before `setup` refused plain HTTP to another machine.
    if api_key.is_some() && in_the_clear(&url) {
        let without_key = match args.api_key {
            Some(_) => "or leave out --api-key if it needs none",
            None => "or run `fair-witness setup` again to use it without the saved key",
        };
        bail!("{}, {without_key}", api_key_unencrypted(&url));
    }
    Ok(Some((AiConfig { url, model, effort }, api_key)))
}

fn read_dmp(path: &Path) -> Result<Dmp> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a maDMP JSON file", path.display()))?;
    Ok(Dmp {
        json: serde_json::to_string_pretty(&json)?,
        status: format!("Comparing changes with the DMP in {}.", path.display()),
    })
}

/// Downloads DMP `id` and keeps a copy of the one chosen in `setup`, which stands in for it
/// when DAMAP cannot be reached. The warning that it does goes into `status`.
async fn download_dmp(
    project: &Project,
    id: i64,
    headless: bool,
    status: &mut Vec<String>,
) -> Result<Dmp> {
    let config = project.config()?.context(NOT_SET_UP)?.damap;
    let downloaded = async {
        let damap = if headless {
            saved_session(project).await?
        } else {
            damap::resume(project, &TerminalPrompts).await?
        };
        let name = damap
            .list_dmps()
            .await?
            .into_iter()
            .find(|listed| listed.id == id)
            .and_then(|listed| listed.name().map(str::to_string))
            .unwrap_or_else(|| "(untitled)".to_string());
        anyhow::Ok((name, damap.madmp(id).await?))
    };
    match downloaded.await {
        Ok((name, madmp)) => {
            // The DMP is downloaded, so a copy that cannot be saved only costs the fallback.
            if config.dmp_id == Some(id)
                && let Err(e) = project.save_madmp(&config.frontend_url, id, &madmp)
            {
                let warning = format!("Warning: could not save a copy of the DMP: {e:#}");
                status.extend(warning.lines().map(str::to_string));
            }
            Ok(Dmp {
                json: serde_json::to_string_pretty(&madmp)?,
                status: format!("Comparing changes with project #{id} - {name}."),
            })
        }
        Err(e) => {
            let Some(json) = project.madmp(&config.frontend_url, id)? else {
                return Err(e);
            };
            let warning = format!(
                "Warning: could not download the DMP, so changes are checked against the copy \
                 saved before: {e:#}"
            );
            tracing::warn!("{warning}");
            // The screen draws each header line as one, and DAMAP's errors span several.
            status.extend(warning.lines().map(str::to_string));
            Ok(Dmp {
                json,
                status: format!("Comparing changes with the saved copy of project #{id}."),
            })
        }
    }
}

/// Logs in to DAMAP with the saved session only, since nobody can log in during a headless run.
async fn saved_session(project: &Project) -> Result<Damap> {
    let config = project.config()?.context(NOT_SET_UP)?;
    let (damap, instance) = damap::reach(&config.damap.frontend_url).await?;
    let oidc = damap::discover(&damap, &instance, &ErrorOutput).await?;
    let login = project
        .credentials()?
        .damap
        .context("not logged in to DAMAP; log in with `fair-witness setup`")?;
    let refresh_token = login
        .refresh_token_for(damap.url(), oidc.issuer())
        .map_err(|e| anyhow!("{e}; log in with `fair-witness setup`"))?;
    let tokens = oidc
        .refresh(refresh_token)
        .await
        .context("the DAMAP login has expired; log in again with `fair-witness setup`")?;
    damap::save_session(project, damap, &oidc, tokens, &ErrorOutput)
}

/// Writes the login's notices to stderr, keeping stdout for events.
struct ErrorOutput;

impl Prompts for ErrorOutput {
    fn notice(&self, message: &str) {
        eprintln!("{message}");
    }

    fn confirm_code(&self, link: &str, code: &str) {
        eprintln!("To log in, open {link} and confirm the code {code}");
    }

    fn username_and_password(&self) -> Result<(String, String)> {
        bail!("cannot ask for a DAMAP login without a terminal; log in with `fair-witness setup`")
    }
}

async fn show_events(start: Start) -> Result<()> {
    let Start {
        project,
        mut records,
        status,
        settings,
    } = start;
    let (updates, received) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(record) = records.recv().await {
            notifications::show(&record.event, &updates);
            if updates.send(Update::Session(record)).is_err() {
                return;
            }
        }
    });
    Screen::new(project.root(), status, settings)
        .run(received)
        .await
}

/// Resolves when stdin closes, on Ctrl+C or on SIGTERM.
fn stop_requested() -> Result<impl Future<Output = ()>> {
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let interrupted = tokio::signal::ctrl_c();
    let stdin_closed =
        tokio::task::spawn_blocking(|| io::copy(&mut io::stdin().lock(), &mut io::sink()));
    Ok(async move {
        #[cfg(unix)]
        let terminated = terminate.recv();
        #[cfg(not(unix))]
        let terminated = std::future::pending::<Option<()>>();
        tokio::select! {
            _ = terminated => {}
            _ = interrupted => {}
            _ = stdin_closed => {}
        }
    })
}

/// Prints each event with its time as a line of JSON on stdout, as the log holds it, until
/// `stopped` resolves.
async fn print_events(start: Start, stopped: impl Future<Output = ()>) -> Result<()> {
    let Start {
        project,
        mut records,
        status,
        ..
    } = start;
    eprintln!("Watching {}", project.root().display());
    for line in &status {
        eprintln!("{line}");
    }
    // Writing on its own thread keeps a reader that stops reading from blocking the stop.
    let (lines, to_write) = std::sync::mpsc::channel::<String>();
    let writer = tokio::task::spawn_blocking(move || {
        let mut stdout = io::stdout();
        for line in to_write {
            stdout.write_all(line.as_bytes())?;
            stdout.flush()?;
        }
        io::Result::Ok(())
    });
    tokio::pin!(stopped, writer);
    loop {
        tokio::select! {
            () = &mut stopped => {
                // Lets the writer finish the lines it has, so a reader gets no half line, but
                // not for long, since a reader that stopped reading never lets it.
                drop(lines);
                // A writer that fails now has lost its reader, which wants no more lines.
                return match tokio::time::timeout(STOP_WRITING_WITHIN, &mut writer).await {
                    Ok(_) => Ok(()),
                    Err(_) => bail!("stopped before writing every event, since stdout was not read"),
                };
            }
            written = &mut writer => {
                written?.context("could not write to stdout")?;
                bail!("stopped writing to stdout");
            }
            record = records.recv() => {
                let record = record.context("stopped watching")?;
                let mut line = serde_json::to_string(&record)?;
                line.push('\n');
                // When the writer has failed, the next turn of the loop reports why.
                let _ = lines.send(line);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use fair_witness_core::project::{ApiKey, Credentials, DamapConfig};

    use super::*;

    fn args(model: Option<&str>, effort: Option<&str>) -> Args {
        Args {
            dmp: None,
            dmp_file: None,
            project: None,
            headless: true,
            ai_url: None,
            api_key: None,
            model: model.map(str::to_string),
            effort: effort.map(|name| name.parse().unwrap()),
            skip_model_check: false,
        }
    }

    #[test]
    fn takes_the_effort_from_the_flag_before_the_saved_one() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::new(dir.path().to_path_buf());
        let config = Config {
            damap: DamapConfig {
                frontend_url: "https://damap.example.org".to_string(),
                dmp_id: None,
            },
            ai: Some(AiConfig {
                url: "https://api.example.org/v1".to_string(),
                model: "small".to_string(),
                effort: "low".parse().unwrap(),
            }),
        };
        let effort = |args: Args| {
            let (ai, _) = ai(&project, Some(&config), &args, &mut Vec::new())
                .unwrap()
                .unwrap();
            ai.effort.name().to_string()
        };

        assert_eq!(
            [
                effort(args(None, None)),
                // The saved effort belongs to the saved model.
                effort(args(Some("large"), None)),
                effort(args(None, Some("high"))),
                effort(args(None, Some("default"))),
                effort(args(Some("large"), Some("none"))),
            ],
            ["low", "default", "high", "default", "none"]
        );
    }

    #[test]
    fn finds_the_settings_saved_for_the_url_however_it_is_typed() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::new(dir.path().to_path_buf());
        project
            .save_credentials(&Credentials {
                damap: None,
                ai: Some(ApiKey {
                    url: "https://api.example.org/v1".to_string(),
                    key: "sk-saved".to_string(),
                }),
            })
            .unwrap();
        let found = |saved: &str, given: &str| {
            let config = Config {
                damap: DamapConfig {
                    frontend_url: "https://damap.example.org".to_string(),
                    dmp_id: None,
                },
                ai: Some(AiConfig {
                    url: saved.to_string(),
                    model: "small".to_string(),
                    effort: "low".parse().unwrap(),
                }),
            };
            let args = Args {
                ai_url: Some(given.to_string()),
                ..args(None, None)
            };
            let (ai, api_key) = ai(&project, Some(&config), &args, &mut Vec::new())
                .unwrap()
                .unwrap();
            (ai.url, ai.model, ai.effort.name().to_string(), api_key)
        };
        let saved = (
            "https://api.example.org/v1".to_string(),
            "small".to_string(),
            "low".to_string(),
            Some("sk-saved".to_string()),
        );

        assert_eq!(
            [
                found("https://api.example.org/v1", "https://api.example.org/v1/"),
                // As an earlier `setup` saved it when typed with a trailing `/`.
                found("https://api.example.org/v1/", "API.example.org/v1"),
            ],
            [saved.clone(), saved]
        );
    }
}
