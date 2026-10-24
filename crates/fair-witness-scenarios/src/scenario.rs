use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fair_witness_core::testing::tool_call;
use fair_witness_core::watcher::Change;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const FILE: &str = "scenario.toml";
pub const DMP: &str = "dmp.json";
pub const SCRIPT: &str = "script.sh";
pub const FILES: &str = "files";

/// A DMP, the files a project starts with, a script of what the researcher does and the
/// verdict expected for each change set the script makes.
pub struct Scenario {
    pub name: String,
    pub dir: PathBuf,
    pub description: String,
    pub sets: Vec<Expected>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioFile {
    description: String,
    #[serde(rename = "set")]
    sets: Vec<Expected>,
}

/// What a change set should hold and the verdict a correct review gives it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    /// The changes the watcher should report, in any order. Left out, they are not compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<Vec<Change>>,
    pub contradicts: bool,
    /// The field a contradiction is in. Left out, any field counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dmp_field: Option<String>,
    /// Why the verdict is right, for the person checking it. The fake model hands it in as its
    /// explanation, and a real model's explanation is not compared with it.
    pub explanation: String,
    /// The tools the fake model calls before it hands in this verdict.
    #[serde(default, skip_serializing)]
    pub fake_tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub tool: String,
    #[serde(default = "no_arguments")]
    pub arguments: Value,
}

fn no_arguments() -> Value {
    json!({})
}

/// The folders in `root` that hold a scenario, by name, as absolute paths, since the script
/// runs in another folder.
pub fn find(root: &Path) -> Result<Vec<PathBuf>> {
    let root = root
        .canonicalize()
        .with_context(|| format!("could not find {}", root.display()))?;
    let entries =
        fs::read_dir(&root).with_context(|| format!("could not read {}", root.display()))?;
    let mut found = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.join(FILE).is_file() {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

impl Scenario {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(FILE);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let file: ScenarioFile =
            toml::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
        if file.sets.is_empty() {
            bail!("{} expects no change set; add a [[set]]", path.display());
        }
        for (number, set) in (1..).zip(&file.sets) {
            if !set.contradicts && set.dmp_field.is_some() {
                bail!(
                    "set {number} in {} has a dmp_field but no contradiction",
                    path.display()
                );
            }
            if set
                .fake_tool_calls
                .iter()
                .any(|call| call.tool == "report_verdict")
            {
                bail!(
                    "set {number} in {} calls report_verdict, which the fake model does on its \
                     own with the expected verdict",
                    path.display()
                );
            }
        }
        let dmp = dir.join(DMP);
        let text = fs::read_to_string(&dmp)
            .with_context(|| format!("could not read {}", dmp.display()))?;
        serde_json::from_str::<Value>(&text)
            .with_context(|| format!("{} is not JSON", dmp.display()))?;
        let script = dir.join(SCRIPT);
        if !script.is_file() {
            bail!("{} is missing", script.display());
        }
        Ok(Self {
            name: dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            dir: dir.to_path_buf(),
            description: file.description,
            sets: file.sets,
        })
    }

    /// What the fake model answers, one reply per request: the tool calls of each set, then
    /// its expected verdict.
    pub fn fake_replies(&self) -> Vec<Value> {
        let mut replies = Vec::new();
        for set in &self.sets {
            for call in &set.fake_tool_calls {
                replies.push(tool_call(&call.tool, call.arguments.clone()));
            }
            let mut verdict = json!({
                "contradicts": set.contradicts,
                "explanation": set.explanation,
            });
            if let Some(field) = &set.dmp_field {
                verdict["dmp_field"] = json!(field);
            }
            replies.push(tool_call("report_verdict", verdict));
        }
        replies
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario(toml: &str) -> (tempfile::TempDir, Result<Scenario>) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE), toml).unwrap();
        fs::write(dir.path().join(DMP), "{}").unwrap();
        fs::write(dir.path().join(SCRIPT), "").unwrap();
        let loaded = Scenario::load(dir.path());
        (dir, loaded)
    }

    #[test]
    fn has_the_fake_model_call_its_tools_before_each_expected_verdict() {
        let (_dir, loaded) = scenario(
            r#"
            description = "Patient names in a project without personal data"

            [[set]]
            contradicts = false
            explanation = "Notes are not data."

            [[set]]
            changes = [{ created = "patients.csv" }]
            contradicts = true
            dmp_field = "dataset[0].personal_data"
            explanation = "The DMP says there is no personal data."
            fake_tool_calls = [{ tool = "read_file", arguments = { path = "patients.csv" } }]
            "#,
        );

        assert_eq!(
            loaded.unwrap().fake_replies(),
            [
                tool_call(
                    "report_verdict",
                    json!({ "contradicts": false, "explanation": "Notes are not data." })
                ),
                tool_call("read_file", json!({ "path": "patients.csv" })),
                tool_call(
                    "report_verdict",
                    json!({
                        "contradicts": true,
                        "dmp_field": "dataset[0].personal_data",
                        "explanation": "The DMP says there is no personal data."
                    })
                ),
            ]
        );
    }

    #[test]
    fn rejects_a_misspelled_field() {
        let (dir, loaded) = scenario(
            r#"
            description = "A typo"

            [[set]]
            contradict = true
            explanation = "Patient names."
            "#,
        );

        let error = format!("{:#}", loaded.err().unwrap());
        let path = dir.path().join(FILE);
        assert!(
            error.starts_with(&format!(
                "{} is not valid: TOML parse error",
                path.display()
            )),
            "{error}"
        );
        assert!(error.contains("unknown field `contradict`"), "{error}");
    }
}
