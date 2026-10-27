use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const DIR_NAME: &str = ".fair-witness";
pub const NOT_SET_UP: &str = "this folder is not set up yet; run `fair-witness setup` first";
pub const IGNORE_FILE: &str = "ignore";
const CONFIG_FILE: &str = "config.toml";
const CREDENTIALS_FILE: &str = "credentials.toml";
const MADMP_FILE: &str = "madmp.json";
/// Ends the name of a file being written, which is renamed over the file it replaces. The
/// ownCloud and Nextcloud clients skip these by default, so a client told to skip
/// `credentials.toml` never uploads half of it either.
const PARTIAL_SUFFIX: &str = ".part";
const LOG_DIR: &str = "log";
const IGNORE_TEMPLATE: &str = "\
# Ignore paths with .gitignore syntax, relative to the project folder.
# Use ! to check a path again, including paths fair-witness skips on its own.
# .fair-witness/ itself is always skipped.
# `fair-witness run` reads this file at startup. Restart it after edits.
# See \"What is ignored\" in the README for what is skipped already:
# https://github.com/vlad-dancea/fair-witness#what-is-ignored
#
# Examples (remove the leading \"# \" to use):
# build/
# scratch/
# *.log
";

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub damap: DamapConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai: Option<AiConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DamapConfig {
    pub frontend_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dmp_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    pub url: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Effort::is_default")]
    pub effort: Effort,
}

/// How much a model reasons: a name the API gets as `reasoning_effort`, or `default`, which sends
/// none so that the model uses its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Effort(String);

const DEFAULT_EFFORT: &str = "default";
/// The efforts the model panel offers. `none` asks for no reasoning, which some models ignore.
const OFFERED_EFFORTS: [&str; 8] = [
    DEFAULT_EFFORT,
    "none",
    "minimal",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
];

impl Effort {
    pub fn offered() -> Vec<Effort> {
        OFFERED_EFFORTS
            .iter()
            .map(|name| Effort(name.to_string()))
            .collect()
    }

    pub fn name(&self) -> &str {
        &self.0
    }

    pub fn is_default(&self) -> bool {
        self.0 == DEFAULT_EFFORT
    }

    /// What goes to the API as `reasoning_effort`, nothing for `default`.
    pub fn sent(&self) -> Option<&str> {
        (!self.is_default()).then_some(&self.0)
    }
}

impl Default for Effort {
    fn default() -> Self {
        Effort(DEFAULT_EFFORT.to_string())
    }
}

impl FromStr for Effort {
    type Err = anyhow::Error;

    // No standard fixes the names, and each model takes its own with its own meaning, so any
    // name passes on as given.
    fn from_str(name: &str) -> Result<Self> {
        if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            bail!(
                "{name:?} is not a reasoning effort; name one such as `low` or `high`, or \
                 `default` to send none"
            );
        }
        Ok(Effort(name.to_string()))
    }
}

impl TryFrom<String> for Effort {
    type Error = anyhow::Error;

    fn try_from(name: String) -> Result<Self> {
        name.parse()
    }
}

impl From<Effort> for String {
    fn from(effort: Effort) -> Self {
        effort.0
    }
}

/// Where a run of `fair-witness run` writes its log, in `.fair-witness/log/`.
#[derive(Debug)]
pub struct RunLog {
    /// Every event of the run, as a line of JSON.
    pub events: PathBuf,
    /// What went wrong or happened outside the reviews, such as requests to DAMAP.
    pub diagnostics: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// A copy of a DMP with where it came from, since DAMAP's maDMP holds neither.
#[derive(Serialize, Deserialize)]
struct SavedMadmp {
    damap_url: String,
    dmp_id: i64,
    madmp: serde_json::Value,
}

/// A project folder: the one that holds `.fair-witness/`.
#[derive(Debug, Clone)]
pub struct Project {
    root: PathBuf,
}

impl Project {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The nearest folder from `dir` upwards that holds `.fair-witness/`.
    pub fn find(dir: &Path) -> Option<Self> {
        dir.ancestors()
            .find(|candidate| candidate.join(DIR_NAME).is_dir())
            .map(|root| Self::new(root.to_path_buf()))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create(&self) -> Result<()> {
        let dir = self.dir();
        fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
        write_gitignore(&dir)?;
        write_ignore(&dir)
    }

    pub fn config(&self) -> Result<Option<Config>> {
        read_toml(&self.dir().join(CONFIG_FILE))
    }

    pub fn save_config(&self, config: &Config) -> Result<()> {
        write_toml(&self.ensure_dir()?, CONFIG_FILE, config, false)
    }

    pub fn credentials(&self) -> Result<Credentials> {
        Ok(read_toml(&self.dir().join(CREDENTIALS_FILE))?.unwrap_or_default())
    }

    pub fn save_credentials(&self, credentials: &Credentials) -> Result<()> {
        write_toml(&self.ensure_dir()?, CREDENTIALS_FILE, credentials, true)
    }

    /// The copy of DMP `dmp_id` from the DAMAP at `damap_url` that an earlier `run` saved, if
    /// the copy there is of that DMP.
    pub fn madmp(&self, damap_url: &str, dmp_id: i64) -> Result<Option<String>> {
        let path = self.dir().join(MADMP_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
        };
        // Copies saved before they said where they came from do not parse, and are not used.
        match serde_json::from_str::<SavedMadmp>(&text) {
            Ok(saved) if saved.damap_url == damap_url && saved.dmp_id == dmp_id => {
                Ok(Some(serde_json::to_string_pretty(&saved.madmp)?))
            }
            _ => Ok(None),
        }
    }

    pub fn save_madmp(
        &self,
        damap_url: &str,
        dmp_id: i64,
        madmp: &serde_json::Value,
    ) -> Result<()> {
        let dir = self.ensure_dir()?;
        write_gitignore(&dir)?;
        let saved = SavedMadmp {
            damap_url: damap_url.to_string(),
            dmp_id,
            madmp: madmp.clone(),
        };
        let text = serde_json::to_string_pretty(&saved)
            .with_context(|| format!("could not write {}", dir.join(MADMP_FILE).display()))?;
        // The DMP names its contributors with their e-mail addresses.
        write_file(&dir, MADMP_FILE, text.as_bytes(), true)
    }

    /// Names the log of a run that starts now, creating its folder and its empty events file.
    pub fn run_log(&self) -> Result<RunLog> {
        self.run_log_at(Timestamp::now())
    }

    fn run_log_at(&self, started: Timestamp) -> Result<RunLog> {
        let dir = self.ensure_dir()?;
        // Folders set up before there was a log have a .gitignore that does not list it.
        write_gitignore(&dir)?;
        let dir = dir.join(LOG_DIR);
        fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
        let started = started.strftime("%Y-%m-%dT%H-%M-%S%.3fZ").to_string();
        // Runs that start in the same millisecond share the time, so creating the events file
        // claims the name, and a later run counts on.
        let mut stem = started.clone();
        for count in 2.. {
            let events = dir.join(format!("{stem}-events.jsonl"));
            match private_options().create_new(true).open(&events) {
                Ok(_) => {
                    return Ok(RunLog {
                        events,
                        diagnostics: dir.join(format!("{stem}-diagnostics.log")),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    stem = format!("{started}-{count}");
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("could not create {}", events.display()));
                }
            }
        }
        unreachable!("the counter runs out only after every name is taken")
    }

    fn dir(&self) -> PathBuf {
        self.root.join(DIR_NAME)
    }

    fn ensure_dir(&self) -> Result<PathBuf> {
        if !self.dir().is_dir() {
            self.create()?;
        }
        Ok(self.dir())
    }
}

fn write_ignore(dir: &Path) -> Result<()> {
    let path = dir.join(IGNORE_FILE);
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("could not create {}", path.display())),
    };
    file.write_all(IGNORE_TEMPLATE.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))
}

fn write_gitignore(dir: &Path) -> Result<()> {
    write_file(
        dir,
        ".gitignore",
        format!("{CREDENTIALS_FILE}\n{MADMP_FILE}\n*{PARTIAL_SUFFIX}\n{LOG_DIR}/\n").as_bytes(),
        false,
    )
}

fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };
    let value =
        toml::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
    Ok(Some(value))
}

/// Creates a file at `path` that only the user can read, failing if one is there.
pub fn create_private(path: &Path) -> Result<File> {
    private_options()
        .create_new(true)
        .open(path)
        .with_context(|| format!("could not create {}", path.display()))
}

/// Opens the file at `path` to add to its end, creating it for the user alone if it is missing.
pub fn append_private(path: &Path) -> Result<File> {
    // Not in append mode, which on Windows takes away the right to cut the file shorter.
    let mut file = private_options()
        .create(true)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    file.seek(SeekFrom::End(0))
        .with_context(|| format!("could not open {}", path.display()))?;
    Ok(file)
}

/// Options to write a file that, when they create it, only the user can read.
fn private_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn write_toml<T: Serialize>(dir: &Path, name: &str, value: &T, private: bool) -> Result<()> {
    let text = toml::to_string(value)
        .with_context(|| format!("could not write {}", dir.join(name).display()))?;
    write_file(dir, name, text.as_bytes(), private)
}

/// Replaces the file `name` in `dir` with `contents`, so that it holds the old contents or the
/// new ones even when writing fails or the computer stops. Afterwards a private file is readable
/// only by the user, whoever could read the one it replaced.
fn write_file(dir: &Path, name: &str, contents: &[u8], private: bool) -> Result<()> {
    replace_file(dir, name, private, |file| file.write_all(contents))
}

fn replace_file(
    dir: &Path,
    name: &str,
    private: bool,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<()> {
    let path = dir.join(name);
    let context = || format!("could not write {}", path.display());
    // The contents go to a new file that is renamed over the old one. The rename also replaces a
    // symlink instead of writing through it, and leaves the mode the new file was created with.
    let prefix = format!("{name}.");
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix).suffix(PARTIAL_SUFFIX);
    if !private {
        // tempfile creates files for the user alone, so the umask has to decide again.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o666));
        }
    }
    // Dropped before the rename, the new file is deleted.
    let mut partial = builder.tempfile_in(dir).with_context(context)?;
    write(partial.as_file_mut()).with_context(context)?;
    // Without the sync, the rename can reach the disk before the contents, and a crash leaves the
    // file empty.
    partial.as_file().sync_all().with_context(context)?;
    partial
        .persist(&path)
        .map_err(|e| e.error)
        .with_context(context)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_the_ignore_template_without_overwriting_an_existing_file() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        let path = root.path().join(DIR_NAME).join(IGNORE_FILE);

        project.create().unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), IGNORE_TEMPLATE);
        for contents in [b"scratch/\n!scratch/keep.csv\n\xff".as_slice(), b""] {
            fs::write(&path, contents).unwrap();

            project.create().unwrap();

            assert_eq!(fs::read(&path).unwrap(), contents);
        }
    }

    #[test]
    fn uses_the_saved_dmp_only_for_the_dmp_and_damap_it_came_from() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        project.create().unwrap();
        let path = root.path().join(DIR_NAME).join(MADMP_FILE);
        // A copy saved before copies said where they came from.
        fs::write(&path, r#"{"title": "old"}"#).unwrap();
        assert_eq!(project.madmp("https://a.example.org", 3).unwrap(), None);

        let madmp = serde_json::json!({ "title": "three" });
        project
            .save_madmp("https://a.example.org", 3, &madmp)
            .unwrap();

        assert_eq!(
            project
                .madmp("https://a.example.org", 3)
                .unwrap()
                .as_deref(),
            Some("{\n  \"title\": \"three\"\n}")
        );
        assert_eq!(project.madmp("https://a.example.org", 4).unwrap(), None);
        assert_eq!(project.madmp("https://b.example.org", 3).unwrap(), None);
    }

    #[test]
    fn gives_runs_that_start_together_their_own_log() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        let started: Timestamp = "2026-10-23T13:48:15.816Z".parse().unwrap();

        let logs: Vec<RunLog> = (0..2)
            .map(|_| project.run_log_at(started).unwrap())
            .collect();

        let log = root.path().join(".fair-witness/log");
        assert_eq!(
            logs.iter()
                .flat_map(|run| [run.events.clone(), run.diagnostics.clone()])
                .collect::<Vec<_>>(),
            [
                log.join("2026-10-23T13-48-15.816Z-events.jsonl"),
                log.join("2026-10-23T13-48-15.816Z-diagnostics.log"),
                log.join("2026-10-23T13-48-15.816Z-2-events.jsonl"),
                log.join("2026-10-23T13-48-15.816Z-2-diagnostics.log"),
            ]
        );
        #[cfg(unix)]
        for run in &logs {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&run.events).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn adds_to_the_end_of_a_log_and_can_cut_it_back() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("events.jsonl");
        fs::write(&path, "{\"set\":1}\n").unwrap();

        let mut file = append_private(&path).unwrap();
        file.write_all(b"{\"set\":2}\n{\"se").unwrap();
        file.set_len(20).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"set\":1}\n{\"set\":2}\n"
        );
    }

    #[test]
    fn finds_the_project_from_a_subfolder() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        let subfolder = root.path().join("data/raw");
        fs::create_dir_all(&subfolder).unwrap();
        assert!(Project::find(&subfolder).is_none());

        project.create().unwrap();

        assert_eq!(Project::find(&subfolder).unwrap().root(), root.path());
    }

    #[test]
    fn loads_the_config_it_saved() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        assert!(project.config().unwrap().is_none());

        project
            .save_config(&Config {
                damap: DamapConfig {
                    frontend_url: "http://localhost:8085".to_string(),
                    dmp_id: Some(7),
                },
                ai: Some(AiConfig {
                    url: "https://api.example.org/v1".to_string(),
                    model: "small".to_string(),
                    effort: "low".parse().unwrap(),
                }),
            })
            .unwrap();

        assert_eq!(
            fs::read_to_string(root.path().join(".fair-witness/config.toml")).unwrap(),
            "[damap]\nfrontend_url = \"http://localhost:8085\"\ndmp_id = 7\n\n\
             [ai]\nurl = \"https://api.example.org/v1\"\nmodel = \"small\"\neffort = \"low\"\n"
        );
        let config = project.config().unwrap().unwrap();
        assert_eq!(config.damap.dmp_id, Some(7));
        assert_eq!(config.ai.unwrap().effort.sent(), Some("low"));
    }

    #[test]
    fn reads_any_effort_name_but_an_empty_one() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        project.create().unwrap();
        let effort = |line: &str| {
            fs::write(
                root.path().join(".fair-witness/config.toml"),
                format!(
                    "[damap]\nfrontend_url = \"http://localhost:8085\"\n\n\
                     [ai]\nurl = \"https://api.example.org/v1\"\nmodel = \"small\"\n{line}"
                ),
            )
            .unwrap();
            match project.config() {
                Ok(config) => Ok(config
                    .unwrap()
                    .ai
                    .unwrap()
                    .effort
                    .sent()
                    .map(str::to_string)),
                Err(e) => Err(format!("{:#}", e.root_cause())),
            }
        };

        assert_eq!(
            effort("effort = \"ultra\"\n"),
            Ok(Some("ultra".to_string()))
        );
        assert_eq!(effort("effort = \"default\"\n"), Ok(None));
        assert_eq!(effort(""), Ok(None));
        let refused = effort("effort = \" low\"\n").unwrap_err();
        assert!(
            refused.contains(
                "\" low\" is not a reasoning effort; name one such as `low` or `high`, or \
                 `default` to send none"
            ),
            "{refused}"
        );
    }

    #[test]
    fn keeps_the_credentials_and_the_dmp_private() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        project.create().unwrap();
        let dir = root.path().join(DIR_NAME);
        // As a sync client or `chmod -R g+rX` leaves them.
        for name in [CREDENTIALS_FILE, MADMP_FILE] {
            fs::write(dir.join(name), "").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dir.join(name), fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
        assert!(project.credentials().unwrap().refresh_token.is_none());

        project
            .save_credentials(&Credentials {
                refresh_token: Some("refresh".to_string()),
                api_key: Some("sk-test".to_string()),
            })
            .unwrap();
        project
            .save_madmp("https://a.example.org", 3, &serde_json::json!({}))
            .unwrap();

        let credentials = project.credentials().unwrap();
        assert_eq!(credentials.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(credentials.api_key.as_deref(), Some("sk-test"));
        assert_eq!(
            fs::read_to_string(dir.join(".gitignore")).unwrap(),
            "credentials.toml\nmadmp.json\n*.part\nlog/\n"
        );
        #[cfg(unix)]
        for name in [CREDENTIALS_FILE, MADMP_FILE] {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(name)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn replaces_a_symlink_instead_of_writing_through_it() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        project.create().unwrap();
        let data = root.path().join("data.csv");
        fs::write(&data, "").unwrap();
        let path = root.path().join(DIR_NAME).join(CREDENTIALS_FILE);
        std::os::unix::fs::symlink(&data, &path).unwrap();

        project
            .save_credentials(&Credentials {
                refresh_token: None,
                api_key: Some("sk-test".to_string()),
            })
            .unwrap();

        assert_eq!(fs::read_to_string(&data).unwrap(), "");
        assert!(fs::symlink_metadata(&path).unwrap().is_file());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "api_key = \"sk-test\"\n"
        );
    }

    #[test]
    fn keeps_the_old_file_when_writing_the_new_one_fails() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        project
            .save_credentials(&Credentials {
                refresh_token: Some("refresh".to_string()),
                api_key: Some("sk-test".to_string()),
            })
            .unwrap();
        let dir = root.path().join(DIR_NAME);
        let path = dir.join(CREDENTIALS_FILE);
        let saved = fs::read_to_string(&path).unwrap();

        // As when the disk fills up halfway through.
        let error = replace_file(&dir, CREDENTIALS_FILE, true, |file| {
            file.write_all(b"refresh_tok")?;
            Err(io::Error::other("no space left on device"))
        })
        .unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            format!(
                "could not write {}: no space left on device",
                path.display()
            )
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, [".gitignore", CREDENTIALS_FILE, IGNORE_FILE]);
    }
}
