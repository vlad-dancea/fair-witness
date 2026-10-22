use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use genai::chat::Tool;
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_ENTRIES: usize = 200;
const MAX_READ: u64 = 16 * 1024;

pub struct Tools {
    root: PathBuf,
}

#[derive(Deserialize)]
struct PathArgs {
    path: String,
}

impl Tools {
    pub fn new(root: &Path) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("could not resolve {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn definitions() -> Vec<Tool> {
        let path_schema = |description: &str| {
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": description }
                },
                "required": ["path"]
            })
        };
        vec![
            Tool::new("list_dir")
                .with_description(
                    "List a folder in the project: subfolders end in `/`, files show their size.",
                )
                .with_schema(path_schema(
                    "Folder relative to the project root; `.` for the root.",
                )),
            Tool::new("read_file")
                .with_description(format!(
                    "Read the start of a file in the project (up to {MAX_READ} bytes). \
                     For binary files, returns the size and first bytes instead."
                ))
                .with_schema(path_schema("File relative to the project root.")),
        ]
    }

    pub fn call(&self, name: &str, args: &Value) -> Result<String, String> {
        let PathArgs { path } =
            serde_json::from_value(args.clone()).map_err(|e| format!("invalid arguments: {e}"))?;
        let path = self.resolve(&path)?;
        match name {
            "list_dir" => list_dir(&path),
            "read_file" => read_file(&path),
            _ => Err(format!("unknown tool `{name}`")),
        }
    }

    fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let resolved = self
            .root
            .join(path)
            .canonicalize()
            .map_err(|e| format!("{path}: {e}"))?;
        let Ok(relative) = resolved.strip_prefix(&self.root) else {
            return Err(format!("{path} is outside the project folder"));
        };
        if relative.starts_with(".fair-witness") {
            return Err(format!("{path} is not part of the research data"));
        }
        Ok(resolved)
    }
}

fn list_dir(path: &Path) -> Result<String, String> {
    let entries = fs::read_dir(path).map_err(|e| e.to_string())?;
    let mut lines: Vec<String> = entries
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            match entry.metadata() {
                Ok(m) if m.is_dir() => format!("{name}/"),
                Ok(m) => format!("{name} ({} bytes)", m.len()),
                Err(_) => name,
            }
        })
        .filter(|line| line != ".fair-witness/")
        .collect();
    lines.sort();
    let total = lines.len();
    lines.truncate(MAX_ENTRIES);
    if total > MAX_ENTRIES {
        lines.push(format!("… and {} more", total - MAX_ENTRIES));
    }
    if lines.is_empty() {
        return Ok("(empty folder)".to_string());
    }
    Ok(lines.join("\n"))
}

fn read_file(path: &Path) -> Result<String, String> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    let mut head = Vec::new();
    file.take(MAX_READ)
        .read_to_end(&mut head)
        .map_err(|e| e.to_string())?;
    if head.contains(&0) {
        let mut magic = String::new();
        for byte in head.iter().take(16) {
            let _ = write!(magic, "{byte:02x} ");
        }
        return Ok(format!(
            "binary file, {size} bytes; starts with: {}",
            magic.trim_end()
        ));
    }
    let mut text = String::from_utf8_lossy(&head).into_owned();
    if size > MAX_READ {
        let _ = write!(text, "\n[truncated: showing {MAX_READ} of {size} bytes]");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, Tools) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("data")).unwrap();
        fs::write(dir.path().join("data/a.csv"), "id,name\n1,x\n").unwrap();
        fs::create_dir(dir.path().join(".fair-witness")).unwrap();
        fs::write(dir.path().join(".fair-witness/credentials.toml"), "secret").unwrap();
        let tools = Tools::new(dir.path()).unwrap();
        (dir, tools)
    }

    fn call(tools: &Tools, name: &str, path: &str) -> Result<String, String> {
        tools.call(name, &json!({ "path": path }))
    }

    #[test]
    fn reads_and_lists_inside_the_project() {
        let (_dir, tools) = project();
        assert_eq!(
            call(&tools, "read_file", "data/a.csv").unwrap(),
            "id,name\n1,x\n"
        );
        assert_eq!(call(&tools, "list_dir", ".").unwrap(), "data/");
        assert_eq!(
            call(&tools, "list_dir", "data").unwrap(),
            "a.csv (12 bytes)"
        );
    }

    #[test]
    #[cfg(unix)]
    fn refuses_paths_outside_the_project() {
        let (dir, tools) = project();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();

        for path in [
            "../",
            "data/../../",
            outside.path().join("secret.txt").to_str().unwrap(),
            "link/secret.txt",
        ] {
            let error = call(&tools, "read_file", path).unwrap_err();
            assert!(error.contains("outside the project"), "{path}: {error}");
        }
    }

    #[test]
    fn refuses_our_own_state() {
        let (_dir, tools) = project();
        assert!(call(&tools, "read_file", ".fair-witness/credentials.toml").is_err());
        assert!(
            call(
                &tools,
                "read_file",
                "data/../.fair-witness/credentials.toml"
            )
            .is_err()
        );
    }

    #[test]
    fn describes_binary_files_instead_of_dumping_them() {
        let (dir, tools) = project();
        fs::write(
            dir.path().join("scan.bin"),
            b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR",
        )
        .unwrap();
        assert_eq!(
            call(&tools, "read_file", "scan.bin").unwrap(),
            "binary file, 16 bytes; starts with: 89 50 4e 47 0d 0a 1a 0a 00 00 00 0d 49 48 44 52",
        );
    }
}
