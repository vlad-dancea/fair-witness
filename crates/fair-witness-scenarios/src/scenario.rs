use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fair_witness_core::review::Verdict;
use fair_witness_core::testing::tool_call;
use fair_witness_core::watcher::Change;
use serde::{Deserialize, Deserializer, Serialize};
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
    /// The fields a contradiction may be in, any of which counts. Left out, any field counts.
    #[serde(
        default,
        deserialize_with = "one_or_more",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub dmp_field: Vec<String>,
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

/// Reads one field or a list of them.
fn one_or_more<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMore {
        One(String),
        More(Vec<String>),
    }
    Ok(match OneOrMore::deserialize(deserializer)? {
        OneOrMore::One(field) => vec![field],
        OneOrMore::More(fields) => fields,
    })
}

impl Expected {
    /// Whether `verdict` gives the expected answer and, for a contradiction, names one of the
    /// expected fields or a field inside one, such as `dataset[0].distribution[0].format[0]`
    /// for `dataset[0].distribution[0].format`.
    pub fn accepts(&self, verdict: &Verdict) -> bool {
        if verdict.contradicts != self.contradicts {
            return false;
        }
        if !verdict.contradicts || self.dmp_field.is_empty() {
            return true;
        }
        let Some(field) = &verdict.dmp_field else {
            return false;
        };
        let field = in_dmp(field);
        self.dmp_field
            .iter()
            .any(|want| field.strip_prefix(in_dmp(want)).is_some_and(path_continues))
    }
}

/// `field` without the `dmp` that every field of a maDMP is inside, which a model or a
/// scenario may name or leave out.
fn in_dmp(field: &str) -> &str {
    let field = field.trim().trim_start_matches("$.");
    field.strip_prefix("dmp.").unwrap_or(field)
}

/// Whether `rest` leads only to fields and items further in, such as `[0].title`, so that a
/// field that names others after the expected one does not count.
fn path_continues(mut rest: &str) -> bool {
    while !rest.is_empty() {
        let (step, after) = if let Some(after) = rest.strip_prefix('.') {
            let end = after
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(after.len());
            after.split_at(end)
        } else if let Some((index, after)) = rest.strip_prefix('[').and_then(|r| r.split_once(']'))
        {
            if !index.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            (index, after)
        } else {
            return false;
        };
        if step.is_empty() {
            return false;
        }
        rest = after;
    }
    true
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
            if !set.contradicts && !set.dmp_field.is_empty() {
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
    /// its expected verdict with the first of its fields.
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
            if let Some(field) = set.dmp_field.first() {
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
            dmp_field = ["dataset[0].personal_data", "dataset[0].sensitive_data"]
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
    fn accepts_the_expected_fields_and_the_fields_inside_them() {
        let expected = Expected {
            changes: None,
            contradicts: true,
            dmp_field: vec![
                "dataset[0].personal_data".to_string(),
                "dmp.dataset[0].distribution[0].format".to_string(),
            ],
            explanation: "Why.".to_string(),
            fake_tool_calls: Vec::new(),
        };
        let accepts = |contradicts: bool, field: Option<&str>| {
            expected.accepts(&Verdict {
                contradicts,
                dmp_field: field.map(str::to_string),
                explanation: "Because.".to_string(),
            })
        };

        assert_eq!(
            [
                accepts(true, Some("dataset[0].personal_data")),
                accepts(true, Some("dmp.dataset[0].personal_data")),
                accepts(true, Some("$.dmp.dataset[0].distribution[0].format[0]")),
                accepts(true, Some("dataset[0].distribution[0].format")),
                accepts(true, Some("dataset[0].personal_data[1].note")),
                accepts(true, Some("dataset[0].distribution[0]")),
                accepts(true, Some("dataset[0].personal_data_note")),
                accepts(true, Some("dataset[1].personal_data")),
                accepts(
                    true,
                    Some("dataset[0].distribution[0].format[0] / dataset[0].title")
                ),
                accepts(true, Some("dataset[0].personal_data[first]")),
                accepts(true, Some("dataset[0].personal_data.")),
                accepts(true, None),
                accepts(false, None),
            ],
            [
                true, true, true, true, true, false, false, false, false, false, false, false,
                false
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
