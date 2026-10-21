use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const DIR_NAME: &str = ".fair-witness";
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

impl Config {
    pub fn load() -> Result<Option<Self>> {
        match find_dir()? {
            Some(dir) => read_toml(&dir.join(CONFIG_FILE)),
            None => Ok(None),
        }
    }

    pub fn save(&self) -> Result<()> {
        write_toml(&ensure_dir()?.join(CONFIG_FILE), self, false)
    }
}

impl Credentials {
    pub fn load() -> Result<Self> {
        let saved = match find_dir()? {
            Some(dir) => read_toml(&dir.join(CREDENTIALS_FILE))?,
            None => None,
        };
        Ok(saved.unwrap_or_default())
    }

    pub fn save(&self) -> Result<()> {
        write_toml(&ensure_dir()?.join(CREDENTIALS_FILE), self, true)
    }
}

pub fn save_madmp(json: &str) -> Result<()> {
    let dir = ensure_dir()?;
    write_gitignore(&dir)?;
    let path = dir.join(MADMP_FILE);
    fs::write(&path, json).with_context(|| format!("could not write {}", path.display()))
}

fn find_dir() -> Result<Option<PathBuf>> {
    let cwd = env::current_dir().context("could not determine the current directory")?;
    Ok(cwd
        .ancestors()
        .map(|dir| dir.join(DIR_NAME))
        .find(|candidate| candidate.is_dir()))
}

pub fn project_root() -> Result<Option<PathBuf>> {
    Ok(find_dir()?.and_then(|dir| dir.parent().map(Path::to_path_buf)))
}

pub fn create_here() -> Result<PathBuf> {
    let dir = env::current_dir()?.join(DIR_NAME);
    fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    write_gitignore(&dir)?;
    Ok(dir)
}

fn write_gitignore(dir: &Path) -> Result<()> {
    let path = dir.join(".gitignore");
    fs::write(&path, format!("{CREDENTIALS_FILE}\n{MADMP_FILE}\n"))
        .with_context(|| format!("could not write {}", path.display()))
}

fn ensure_dir() -> Result<PathBuf> {
    match find_dir()? {
        Some(dir) => Ok(dir),
        None => create_here(),
    }
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
