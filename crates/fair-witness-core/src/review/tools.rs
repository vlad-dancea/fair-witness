use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use genai::chat::Tool;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::escape;
use crate::watcher::Filter;

const MAX_ENTRIES: usize = 200;
const MAX_READ: u64 = 16 * 1024;

pub struct Tools {
    root: PathBuf,
    filter: Filter,
}

#[derive(Deserialize)]
struct PathArgs {
    path: String,
}

impl Tools {
    /// Tools that read only inside `root` and only what `filter` keeps.
    pub fn new(root: &Path, filter: Filter) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("could not resolve {}", root.display()))?;
        Ok(Self { root, filter })
    }

    pub fn definitions() -> Vec<Tool> {
        let path_schema = |description: &str| {
            let description = format!(
                "{description} Write names as the list of changes and list_dir show them, \
                 escapes included."
            );
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
        let relative = self.resolve(&path)?;
        match name {
            "list_dir" => self.list_dir(&relative),
            "read_file" => read_file(&self.root.join(relative)),
            _ => Err(format!("unknown tool `{name}`")),
        }
    }

    /// Where `path` leads, relative to the root, if that is research data. Both the name, which
    /// the watcher and `list_dir` judge, and where a link in it leads must be.
    fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let unescaped = escape::unescape(path).map_err(|e| format!("{path}: {e}"))?;
        let resolved = self
            .root
            .join(&unescaped)
            .canonicalize()
            .map_err(|e| format!("{path}: {e}"))?;
        let Ok(relative) = resolved.strip_prefix(&self.root) else {
            return Err(format!("{path} is outside the project folder"));
        };
        let skips_name = as_written(&self.root, &unescaped).is_some_and(|name| {
            // Without following a link, as the watcher looks at it.
            let is_dir = fs::symlink_metadata(self.root.join(&name)).is_ok_and(|m| m.is_dir());
            self.filter.skips_relative(&name, is_dir)
        });
        if skips_name || self.filter.skips_relative(relative, resolved.is_dir()) {
            return Err(format!("{path} is not part of the research data"));
        }
        Ok(relative.to_path_buf())
    }

    fn list_dir(&self, folder: &Path) -> Result<String, String> {
        let entries = fs::read_dir(self.root.join(folder)).map_err(|e| e.to_string())?;
        let mut lines: Vec<String> = entries
            .flatten()
            .filter_map(|entry| {
                // Without following a link, as the watcher looks at it.
                let metadata = entry.metadata();
                let is_dir = metadata.as_ref().is_ok_and(fs::Metadata::is_dir);
                if self
                    .filter
                    .skips_relative(&folder.join(entry.file_name()), is_dir)
                {
                    return None;
                }
                let name = escape::path(Path::new(&entry.file_name()));
                Some(match metadata {
                    Ok(m) if m.is_dir() => format!("{name}/"),
                    Ok(m) => format!("{name} ({} bytes)", m.len()),
                    Err(_) => name,
                })
            })
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
}

/// `path` relative to `root` as written, with `.` and `..` applied to the name alone, or
/// nothing when the name leaves `root`.
fn as_written(root: &Path, path: &Path) -> Option<PathBuf> {
    let mut name = PathBuf::new();
    for component in path.strip_prefix(root).unwrap_or(path).components() {
        match component {
            Component::Normal(part) => name.push(part),
            Component::CurDir => {}
            Component::ParentDir if name.pop() => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(name)
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
        for (path, contents) in [
            ("data/a.csv", "id,name\n1,x\n"),
            (".fair-witness/credentials.toml", "secret"),
            (".fair-witness/ignore", "scratch/\nprivate.csv\n"),
            // As `fair-witness setup` in `data/` leaves it.
            ("data/.fair-witness/credentials.toml", "secret"),
            (".git/config", "url = https://token@example.org/lab.git\n"),
            ("scratch/tmp.csv", "id\n"),
        ] {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        let filter = Filter::new(dir.path()).unwrap();
        let tools = Tools::new(dir.path(), filter).unwrap();
        (dir, tools)
    }

    fn call(tools: &Tools, name: &str, path: &str) -> Result<String, String> {
        tools.call(name, &json!({ "path": path }))
    }

    #[test]
    fn reads_and_lists_the_research_data() {
        let (_dir, tools) = project();
        assert_eq!(
            call(&tools, "read_file", "data/a.csv").unwrap(),
            "id,name\n1,x\n"
        );
        // Without `.fair-witness/`, `.git/`, `scratch/` or `data/.fair-witness/`.
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
            "\\u{2e}\\u{2e}/",
        ] {
            let error = call(&tools, "read_file", path).unwrap_err();
            assert!(error.contains("outside the project"), "{path}: {error}");
        }
    }

    #[test]
    fn refuses_what_the_watcher_skips() {
        let (_dir, tools) = project();
        for path in [
            ".fair-witness/credentials.toml",
            "data/../.fair-witness/credentials.toml",
            ".fair-witnes\\u{73}/credentials.toml",
            "data/.fair-witness/credentials.toml",
            "data/.fair-witness",
            ".git/config",
            "scratch/tmp.csv",
        ] {
            for tool in ["read_file", "list_dir"] {
                assert_eq!(
                    call(&tools, tool, path),
                    Err(format!("{path} is not part of the research data")),
                    "{tool}"
                );
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn refuses_a_link_to_or_named_as_what_the_watcher_skips() {
        let (dir, tools) = project();
        fs::create_dir(dir.path().join("notes")).unwrap();
        for (target, link) in [
            ("../.git/config", "data/remote"),
            ("data/a.csv", "private.csv"),
            ("../data", "notes/.fair-witness"),
        ] {
            std::os::unix::fs::symlink(target, dir.path().join(link)).unwrap();
        }

        for path in ["data/remote", "private.csv", "notes/.fair-witness/a.csv"] {
            assert_eq!(
                call(&tools, "read_file", path),
                Err(format!("{path} is not part of the research data"))
            );
        }
        assert_eq!(call(&tools, "list_dir", ".").unwrap(), "data/\nnotes/");
        assert_eq!(call(&tools, "list_dir", "notes").unwrap(), "(empty folder)");
    }

    #[test]
    #[cfg(unix)]
    fn reads_a_file_by_the_name_list_dir_shows() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let (dir, tools) = project();
        fs::write(dir.path().join("data/two\nlines.csv"), "id\n2\n").unwrap();
        fs::write(dir.path().join("data/back\\n.csv"), "id\n3\n").unwrap();
        let latin1 = OsStr::from_bytes(b"latin1-\xe9t\xe9.csv");
        fs::write(dir.path().join("data").join(latin1), "id\n4\n").unwrap();

        assert_eq!(
            call(&tools, "list_dir", "data").unwrap(),
            "a.csv (12 bytes)\nback\\\\n.csv (5 bytes)\nlatin1-\\xe9t\\xe9.csv (5 bytes)\n\
             two\\nlines.csv (5 bytes)"
        );
        assert_eq!(
            call(&tools, "read_file", "data/latin1-\\xe9t\\xe9.csv").unwrap(),
            "id\n4\n"
        );
        assert_eq!(
            call(&tools, "read_file", "data/two\\nlines.csv").unwrap(),
            "id\n2\n"
        );
        assert_eq!(
            call(&tools, "read_file", "data/two\nlines.csv").unwrap(),
            "id\n2\n"
        );
        assert_eq!(
            call(&tools, "read_file", "data/back\\\\n.csv").unwrap(),
            "id\n3\n"
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
