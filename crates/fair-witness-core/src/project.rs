use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jiff::Timestamp;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const DIR_NAME: &str = ".fair-witness";
pub const NOT_SET_UP: &str = "this folder is not set up yet; run `fair-witness setup` first";
pub const IGNORE_FILE: &str = "ignore";
const CONFIG_FILE: &str = "config.toml";
const CREDENTIALS_FILE: &str = "credentials.toml";
const MADMP_FILE: &str = "madmp.json";
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    None,
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn name(self) -> &'static str {
        match self {
            Effort::None => "none",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
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
        write_toml(&self.ensure_dir()?.join(CONFIG_FILE), config, false)
    }

    pub fn credentials(&self) -> Result<Credentials> {
        Ok(read_toml(&self.dir().join(CREDENTIALS_FILE))?.unwrap_or_default())
    }

    pub fn save_credentials(&self, credentials: &Credentials) -> Result<()> {
        write_toml(
            &self.ensure_dir()?.join(CREDENTIALS_FILE),
            credentials,
            true,
        )
    }

    pub fn save_madmp(&self, json: &str) -> Result<()> {
        let dir = self.ensure_dir()?;
        write_gitignore(&dir)?;
        let path = dir.join(MADMP_FILE);
        fs::write(&path, json).with_context(|| format!("could not write {}", path.display()))
    }

    /// Names the log of a run that starts now and creates its folder.
    pub fn run_log(&self) -> Result<RunLog> {
        let dir = self.ensure_dir()?;
        // Folders set up before there was a log have a .gitignore that does not list it.
        write_gitignore(&dir)?;
        let dir = dir.join(LOG_DIR);
        fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
        let started = Timestamp::now().strftime("%Y-%m-%dT%H-%M-%S%.3fZ");
        Ok(RunLog {
            events: dir.join(format!("{started}-events.jsonl")),
            diagnostics: dir.join(format!("{started}-diagnostics.log")),
        })
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
    let path = dir.join(".gitignore");
    fs::write(
        &path,
        format!("{CREDENTIALS_FILE}\n{MADMP_FILE}\n{LOG_DIR}/\n"),
    )
    .with_context(|| format!("could not write {}", path.display()))
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
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    only_for_the_user(&mut options);
    options
        .open(path)
        .with_context(|| format!("could not create {}", path.display()))
}

fn only_for_the_user(options: &mut fs::OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = options;
}

fn write_toml<T: Serialize>(path: &Path, value: &T, private: bool) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    if private {
        only_for_the_user(&mut options);
    }

    let mut file = options
        .open(path)
        .with_context(|| format!("could not write {}", path.display()))?;
    file.write_all(toml::to_string(value)?.as_bytes())?;
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
                    effort: Some(Effort::Low),
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
        assert_eq!(config.ai.unwrap().effort, Some(Effort::Low));
    }

    #[test]
    fn keeps_the_credentials_private() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        assert!(project.credentials().unwrap().refresh_token.is_none());

        project
            .save_credentials(&Credentials {
                refresh_token: Some("refresh".to_string()),
                api_key: Some("sk-test".to_string()),
            })
            .unwrap();

        let credentials = project.credentials().unwrap();
        assert_eq!(credentials.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(credentials.api_key.as_deref(), Some("sk-test"));
        assert_eq!(
            fs::read_to_string(root.path().join(".fair-witness/.gitignore")).unwrap(),
            "credentials.toml\nmadmp.json\nlog/\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = root.path().join(".fair-witness/credentials.toml");
            let mode = fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
