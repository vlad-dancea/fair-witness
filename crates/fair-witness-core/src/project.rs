use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const DIR_NAME: &str = ".fair-witness";
pub const NOT_SET_UP: &str = "this folder is not set up yet; run `fair-witness setup` first";
pub const IGNORE_FILE: &str = "ignore";
const CONFIG_FILE: &str = "config.toml";
const CREDENTIALS_FILE: &str = "credentials.toml";
const MADMP_FILE: &str = "madmp.json";

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
        write_gitignore(&dir)
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

fn write_gitignore(dir: &Path) -> Result<()> {
    let path = dir.join(".gitignore");
    fs::write(&path, format!("{CREDENTIALS_FILE}\n{MADMP_FILE}\n"))
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

fn write_toml<T: Serialize>(path: &Path, value: &T, private: bool) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;

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
            "credentials.toml\nmadmp.json\n"
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
