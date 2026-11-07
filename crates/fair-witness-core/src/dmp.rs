//! The DMP a session reviews against, and the places it comes from.

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::auth::Notices;
use crate::damap::{self, Login};
use crate::net::same_url;
use crate::project::Project;

/// Where the text of a DMP came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DmpOrigin {
    Downloaded,
    SavedCopy,
    File,
}

/// Which version of a DMP a review used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmpVersion {
    pub origin: DmpOrigin,
    /// When the plan last changed, as the maDMP says. DAMAP leaves it out until the first edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// The start of the SHA-256 of the text.
    pub hash: String,
}

/// The text of a DMP, with what a person and the log are told about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Dmp {
    pub text: String,
    pub version: DmpVersion,
    /// The header line that names it.
    pub status: String,
    /// What went wrong without keeping it from being used.
    pub warning: Option<String>,
}

impl Dmp {
    pub fn from_madmp(madmp: &Value, origin: DmpOrigin, status: String) -> Result<Self> {
        let text = serde_json::to_string_pretty(madmp)?;
        let modified = plan(madmp)
            .get("modified")
            .and_then(Value::as_str)
            .map(str::to_string);
        let hash = short_hash(&text);
        Ok(Self {
            text,
            version: DmpVersion {
                origin,
                modified,
                hash,
            },
            status,
            warning: None,
        })
    }
}

/// The plan in `madmp`: DAMAP's API answers with it as it is, and the RDA standard's files hold it
/// under `dmp`.
fn plan(madmp: &Value) -> &Value {
    madmp.get("dmp").unwrap_or(madmp)
}

fn short_hash(text: &str) -> String {
    Sha256::digest(text)
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The warning that a DMP could not be fetched because of `why`, so that reviews go on with the
/// text they have, from `origin`.
pub(crate) fn kept_warning(origin: DmpOrigin, why: &anyhow::Error) -> String {
    let kept = match origin {
        DmpOrigin::Downloaded => {
            "could not download the DMP, so changes are checked against the \
                                  last version downloaded"
        }
        DmpOrigin::SavedCopy => {
            "could not download the DMP, so changes are checked against the \
                                 copy saved before"
        }
        DmpOrigin::File => {
            "could not read the DMP, so changes are checked against the last \
                            version read"
        }
    };
    format!("Warning: {kept}: {why:#}")
}

pub type DmpFuture<'a> = Pin<Box<dyn Future<Output = Result<Dmp>> + Send + 'a>>;

/// Where a session gets the DMP before each review.
pub trait DmpSource: Send + Sync {
    /// The DMP as it is now. Nobody can answer a prompt or read a line by the time a session asks,
    /// so it says what went wrong only through its error.
    fn fetch(&self) -> DmpFuture<'_>;
}

/// The maDMP in a file, as `--dmp-file` names it.
#[derive(Clone)]
pub struct FileDmp {
    path: PathBuf,
    shown: PathBuf,
}

impl FileDmp {
    /// The file `path` as seen from the folder `here` that a run started in.
    pub fn new(here: &Path, path: &Path) -> Self {
        Self {
            path: here.join(path),
            shown: path.to_path_buf(),
        }
    }

    pub fn read(&self) -> Result<Dmp> {
        let shown = self.shown.display();
        let text =
            fs::read_to_string(&self.path).with_context(|| format!("could not read {shown}"))?;
        let madmp: Value = serde_json::from_str(&text)
            .with_context(|| format!("{shown} is not a maDMP JSON file"))?;
        let status = format!("Comparing changes with the DMP in {shown}.");
        Dmp::from_madmp(&madmp, DmpOrigin::File, status)
    }
}

impl DmpSource for FileDmp {
    fn fetch(&self) -> DmpFuture<'_> {
        let file = self.clone();
        // On a thread of its own, so that a file on a network drive that hangs can only use up
        // the time a session gives it.
        Box::pin(async move { tokio::task::spawn_blocking(move || file.read()).await? })
    }
}

/// DMP `id` on the DAMAP at `url`, for the folder `project`.
pub struct DamapDmp {
    project: Project,
    url: String,
    id: i64,
}

impl DamapDmp {
    pub fn new(project: Project, url: String, id: i64) -> Self {
        Self { project, url, id }
    }

    /// Downloads the DMP, logging in as `login` allows, and keeps a copy of it when it is the one
    /// chosen in `setup`, which can differ from the one this run started with by now.
    pub async fn download(&self, login: Login<'_>) -> Result<Dmp> {
        let (damap, instance) = damap::reach(&self.url).await?;
        // Read each time, since the last refresh or a `setup` or `logout` meanwhile changed it.
        let saved = self.project.credentials()?.damap;
        let damap = damap::log_in(&self.project, damap, &instance, saved.as_ref(), login).await?;
        let madmp = damap.madmp(self.id).await?;
        let status = format!(
            "Comparing changes with project #{} - {}.",
            self.id,
            title(&madmp).unwrap_or("(untitled)")
        );
        let mut dmp = Dmp::from_madmp(&madmp, DmpOrigin::Downloaded, status)?;
        // The DMP is downloaded, so a copy that cannot be saved only costs the fallback.
        if self.is_chosen()
            && let Err(e) = self.project.save_madmp(&self.url, self.id, &madmp)
        {
            dmp.warning = Some(format!("Warning: could not save a copy of the DMP: {e:#}"));
        }
        Ok(dmp)
    }

    /// The copy of the DMP an earlier run saved, if there is one, in place of the one that could
    /// not be downloaded because of `why`.
    pub fn saved_copy(&self, why: &anyhow::Error) -> Result<Option<Dmp>> {
        let Some(text) = self.project.madmp(&self.url, self.id)? else {
            return Ok(None);
        };
        let madmp: Value = serde_json::from_str(&text)?;
        let status = format!(
            "Comparing changes with the saved copy of project #{}.",
            self.id
        );
        let mut dmp = Dmp::from_madmp(&madmp, DmpOrigin::SavedCopy, status)?;
        dmp.warning = Some(kept_warning(DmpOrigin::SavedCopy, why));
        Ok(Some(dmp))
    }

    fn is_chosen(&self) -> bool {
        matches!(
            self.project.config(),
            Ok(Some(config)) if config.damap.dmp_id == Some(self.id)
                && same_url(&config.damap.frontend_url, &self.url)
        )
    }
}

impl DmpSource for DamapDmp {
    fn fetch(&self) -> DmpFuture<'_> {
        Box::pin(self.download(Login::SavedOnly(&Quiet)))
    }
}

/// The project's title in `madmp`, the same one DAMAP's list of DMPs shows, or else the DMP's.
fn title(madmp: &Value) -> Option<&str> {
    let plan = plan(madmp);
    let project = plan.pointer("/project/0/title").and_then(Value::as_str);
    let dmp = plan.get("title").and_then(Value::as_str);
    [project, dmp]
        .into_iter()
        .flatten()
        .find(|title| !title.trim().is_empty())
}

/// Tells nobody how a login goes, since a session's reviews run under the screen.
struct Quiet;

impl Notices for Quiet {
    fn notice(&self, _: &str) {}
}

#[cfg(test)]
mod tests {
    use std::thread::JoinHandle;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::project::{Config, Credentials, DamapConfig, DamapLogin};
    use crate::testing::{Download, FakeDamap, Request, answer, fake_damap};

    const ID: i64 = 3;

    /// A folder set up for DMP `chosen` on `damap`, and logged in there.
    fn folder(damap: &FakeDamap, chosen: i64) -> (TempDir, Project) {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        project
            .save_config(&Config {
                damap: DamapConfig {
                    frontend_url: damap.url.clone(),
                    dmp_id: Some(chosen),
                },
                ai: None,
            })
            .unwrap();
        project
            .save_credentials(&Credentials {
                damap: Some(DamapLogin {
                    url: damap.url.clone(),
                    login_server: Some(damap.login_server.clone()),
                    refresh_token: "saved".to_string(),
                }),
                ai: None,
            })
            .unwrap();
        (root, project)
    }

    /// The refresh token each token request sent.
    fn sent_tokens(requests: JoinHandle<Vec<Request>>) -> Vec<String> {
        requests
            .join()
            .unwrap()
            .iter()
            .map(|(_, body)| {
                let form = body.as_str().unwrap();
                let token = form
                    .split('&')
                    .find_map(|field| field.strip_prefix("refresh_token="));
                token.unwrap().to_string()
            })
            .collect()
    }

    #[tokio::test]
    async fn keeps_the_refreshed_session_for_the_next_start_and_the_copy_current() {
        let madmp = json!({
            "project": [{ "title": "Soil samples" }],
            "modified": "2026-11-07T10:00:00.000+00:00",
        });
        let damap = fake_damap(vec![
            Download::Gives("first", madmp.clone()),
            Download::Gives("second", madmp.clone()),
        ]);
        let (_root, project) = folder(&damap, ID);

        // Each its own value, as a run that started again has.
        let during_a_run = DamapDmp::new(project.clone(), damap.url.clone(), ID);
        let dmp = during_a_run.fetch().await.unwrap();
        let next_start = DamapDmp::new(project.clone(), damap.url.clone(), ID);
        next_start.download(Login::SavedOnly(&Quiet)).await.unwrap();

        let text = serde_json::to_string_pretty(&madmp).unwrap();
        assert_eq!(
            dmp,
            Dmp {
                version: DmpVersion {
                    origin: DmpOrigin::Downloaded,
                    modified: Some("2026-11-07T10:00:00.000+00:00".to_string()),
                    hash: short_hash(&text),
                },
                text: text.clone(),
                status: "Comparing changes with project #3 - Soil samples.".to_string(),
                warning: None,
            }
        );
        assert_eq!(sent_tokens(damap.token_requests), ["saved", "first"]);
        assert_eq!(
            project.credentials().unwrap().damap.unwrap().refresh_token,
            "second"
        );
        assert_eq!(project.madmp(&damap.url, ID).unwrap(), Some(text));
    }

    #[tokio::test]
    async fn saves_no_copy_of_a_dmp_that_is_no_longer_the_chosen_one() {
        let damap = fake_damap(vec![Download::Gives(
            "first",
            json!({ "project": [], "title": "Another plan" }),
        )]);
        let (_root, project) = folder(&damap, ID + 1);

        let dmp = DamapDmp::new(project.clone(), damap.url.clone(), ID)
            .fetch()
            .await
            .unwrap();

        assert_eq!(
            (dmp.status.as_str(), dmp.warning),
            ("Comparing changes with project #3 - Another plan.", None)
        );
        assert_eq!(project.madmp(&damap.url, ID).unwrap(), None);
    }

    #[tokio::test]
    async fn warns_when_it_downloaded_a_dmp_but_cannot_save_its_copy() {
        let damap = fake_damap(vec![Download::Gives("first", json!({ "project": [] }))]);
        let (root, project) = folder(&damap, ID);
        // A folder under the name of the copy, which no rename can replace.
        fs::create_dir(root.path().join(".fair-witness/madmp.json")).unwrap();

        let dmp = DamapDmp::new(project, damap.url.clone(), ID)
            .fetch()
            .await
            .unwrap();

        let warning = dmp.warning.unwrap();
        assert!(
            warning.starts_with("Warning: could not save a copy of the DMP: could not write"),
            "{warning}"
        );
        assert_eq!(
            dmp.status,
            "Comparing changes with project #3 - (untitled)."
        );
    }

    #[tokio::test]
    async fn says_why_it_cannot_download_without_asking_anyone() {
        let expired = answer(
            "400 Bad Request",
            json!({ "error": "invalid_grant", "error_description": "Token is not active" }),
        );
        let damap = fake_damap(vec![Download::Refused(expired), Download::Down]);
        let (_root, project) = folder(&damap, ID);
        let source = DamapDmp::new(project.clone(), damap.url.clone(), ID);

        let expired = source.fetch().await.unwrap_err();
        let down = source.fetch().await.unwrap_err();

        assert_eq!(
            format!("{expired:#}"),
            format!(
                "the DAMAP login has expired; log in again with `fair-witness setup`: session \
                 could not be refreshed: {} answered 400 Bad Request: Token is not active \
                 (invalid_grant)",
                damap.token_endpoint
            )
        );
        assert_eq!(sent_tokens(damap.token_requests), ["saved"]);
        assert_eq!(
            project.credentials().unwrap().damap.unwrap().refresh_token,
            "saved"
        );
        assert!(
            format!("{down:#}").starts_with(&format!("could not reach {}", damap.url)),
            "{down:#}"
        );
    }

    #[test]
    fn reads_the_time_a_plan_changed_from_either_shape_of_madmp() {
        let modified = |madmp: Value| {
            Dmp::from_madmp(&madmp, DmpOrigin::File, String::new())
                .unwrap()
                .version
                .modified
        };
        let time = "2026-11-07T10:00:00.000+00:00";

        assert_eq!(
            [
                modified(json!({ "modified": time })),
                modified(json!({ "dmp": { "modified": time } })),
                modified(json!({ "created": time })),
            ],
            [Some(time.to_string()), Some(time.to_string()), None]
        );
    }

    #[test]
    fn gives_the_hash_of_the_text_as_sha256sum_does() {
        let dmp = Dmp::from_madmp(&json!({}), DmpOrigin::File, String::new()).unwrap();

        // `printf '{}' | sha256sum`
        assert_eq!(
            (dmp.text.as_str(), dmp.version.hash.as_str()),
            ("{}", "44136fa355b3")
        );
    }

    #[tokio::test]
    async fn reads_its_file_again_each_time_and_says_what_is_wrong_with_it() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("dmp.json");
        let source = FileDmp::new(root.path(), Path::new("dmp.json"));

        fs::write(&file, r#"{"dmp": {"title": "one"}}"#).unwrap();
        let first = source.fetch().await.unwrap();
        fs::write(&file, r#"{"dmp": {"title": "two"}}"#).unwrap();
        let second = source.fetch().await.unwrap();
        fs::write(&file, r#"{"dmp": {"tit"#).unwrap();
        let half_saved = source.fetch().await.unwrap_err();
        fs::remove_file(&file).unwrap();
        let gone = source.fetch().await.unwrap_err();

        assert!(first.text.contains("one") && second.text.contains("two"));
        assert_ne!(first.version.hash, second.version.hash);
        assert_eq!(first.status, "Comparing changes with the DMP in dmp.json.");
        assert!(
            format!("{half_saved:#}").starts_with("dmp.json is not a maDMP JSON file: "),
            "{half_saved:#}"
        );
        assert!(
            format!("{gone:#}").starts_with("could not read dmp.json: "),
            "{gone:#}"
        );
    }
}
