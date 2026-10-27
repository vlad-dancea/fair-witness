use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use genai::chat::Tool;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::escape;
use crate::watcher::Filter;

const MAX_ENTRIES: usize = 200;
const MAX_READ: u64 = 16 * 1024;
/// As many links as Linux follows in one path, so that a loop of links ends.
const MAX_LINKS: usize = 40;

pub struct Tools {
    root: PathBuf,
    /// The project folder, opened once, so that a link in its place later is not followed.
    folder: Folder,
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
        let folder =
            Folder::open(&root).with_context(|| format!("could not open {}", root.display()))?;
        if !folder.is_at(&root) {
            bail!("{} was replaced while it was opened", root.display());
        }
        Ok(Self {
            root,
            folder,
            filter,
        })
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
        let found = self.find(&path)?;
        match name {
            "list_dir" => self.list_dir(&path, found),
            "read_file" => read_file(&path, found.entry),
            _ => Err(format!("unknown tool `{name}`")),
        }
    }

    /// What `path` leads to, if that is research data. Both the name, which the watcher and
    /// `list_dir` judge, and where a link in it leads must be.
    fn find(&self, path: &str) -> Result<Found, String> {
        let found = self.walk(path)?;
        let is_dir = matches!(found.entry, Entry::Folder(_));
        if self
            .filter
            .skips_relative(&found.name, is_dir && !found.name_is_link)
            || self.filter.skips_relative(&found.real, is_dir)
        {
            return Err(format!("{path} is not part of the research data"));
        }
        Ok(found)
    }

    /// Follows `path` from the project folder one name at a time, without letting the system
    /// follow a link: each folder is opened before the next name is looked up in it, and each
    /// link is followed here. What is judged is then what was opened, whatever another process
    /// renames meanwhile.
    fn walk(&self, path: &str) -> Result<Found, String> {
        let failed = |e: io::Error| format!("{path}: {e}");
        let outside = || format!("{path} is outside the project folder");
        let unescaped = escape::unescape(path).map_err(|e| format!("{path}: {e}"))?;
        let written = if unescaped.is_absolute() {
            unescaped.strip_prefix(&self.root).map_err(|_| outside())?
        } else {
            &unescaped
        };
        // Each step with whether it was written, so that its name counts, or comes from a link.
        let mut pending: VecDeque<(Step, bool)> = steps(written)
            .ok_or_else(outside)?
            .into_iter()
            .map(|step| (step, true))
            .collect();
        // The filter looks for cache tags by path, so it judges what the walk opens only while
        // the project folder is still at its path.
        if !self.folder.is_at(&self.root) {
            return Err(format!(
                "{path}: the project folder was moved or replaced since the review started"
            ));
        }
        let mut folders = vec![self.folder.try_clone().map_err(failed)?];
        let mut name = PathBuf::new();
        let mut name_is_link = false;
        let mut real = PathBuf::new();
        let mut end = None;
        let mut links = 0;
        while let Some((step, written)) = pending.pop_front() {
            if end.is_some() {
                return Err(format!("{path}: not a folder"));
            }
            match step {
                Step::Parent => {
                    if !real.pop() {
                        return Err(outside());
                    }
                    folders.pop();
                    // From the folder a link led to, `..` leads to that folder's parent, not
                    // to the link's, so the name continues from there.
                    if written {
                        name.clone_from(&real);
                        name_is_link = false;
                    }
                }
                Step::Name(part) => {
                    let folder = folders.last().expect("the project folder stays open");
                    let entry = folder.entry(&part).map_err(failed)?;
                    // A file system that ignores case or Unicode normalization, as macOS and
                    // Windows do by default, also finds `.fair-witness` as `.FAIR-WITNESS`,
                    // which the filter would not recognize.
                    if !folder.names().map_err(failed)?.contains(&part) {
                        return Err(format!(
                            "{path}: nothing in its folder is named exactly {}, write names as \
                             list_dir shows them",
                            escape::path(Path::new(&part))
                        ));
                    }
                    if written {
                        name.push(&part);
                        name_is_link = matches!(entry, Entry::Link(_));
                    }
                    match entry {
                        Entry::Link(target) => {
                            links += 1;
                            if links > MAX_LINKS {
                                return Err(format!("{path}: too many links"));
                            }
                            let target = if target.is_absolute() {
                                // Canonical like the project folder, so that a link through
                                // another name for it, such as /var for /private/var on
                                // macOS, still leads inside.
                                let target = target.canonicalize().map_err(failed)?;
                                folders.truncate(1);
                                real.clear();
                                target
                                    .strip_prefix(&self.root)
                                    .map_err(|_| outside())?
                                    .to_path_buf()
                            } else {
                                target
                            };
                            for step in steps(&target).ok_or_else(outside)?.into_iter().rev() {
                                pending.push_front((step, false));
                            }
                        }
                        Entry::Folder(folder) => {
                            folders.push(folder);
                            real.push(&part);
                        }
                        entry => {
                            real.push(&part);
                            end = Some(entry);
                        }
                    }
                }
            }
        }
        let entry = match end {
            Some(entry) => entry,
            None => Entry::Folder(folders.pop().expect("the project folder stays open")),
        };
        Ok(Found {
            name,
            name_is_link,
            real,
            entry,
        })
    }

    fn list_dir(&self, path: &str, found: Found) -> Result<String, String> {
        let Entry::Folder(folder) = found.entry else {
            return Err(format!("{path} is not a folder"));
        };
        let mut lines: Vec<String> = folder
            .list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter_map(|(entry, kind)| {
                // Without following a link, as the watcher looks at it, and by both names, as
                // `find` judges the folder.
                let is_dir = matches!(kind, Some(Kind::Folder));
                let skips =
                    |parent: &Path| self.filter.skips_relative(&parent.join(&entry), is_dir);
                if skips(&found.name) || (found.real != found.name && skips(&found.real)) {
                    return None;
                }
                let name = escape::path(Path::new(&entry));
                Some(match kind {
                    Some(Kind::Folder) => format!("{name}/"),
                    Some(Kind::File(size)) => format!("{name} ({size} bytes)"),
                    Some(Kind::Other) => format!("{name} (not a regular file)"),
                    None => name,
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

/// A path the tools walked.
struct Found {
    /// The path as written, relative to the project folder, which is how the watcher and
    /// `list_dir` name it.
    name: PathBuf,
    /// Whether `name` ends in a link, which the watcher does not judge as a folder.
    name_is_link: bool,
    /// Where the path leads, relative to the project folder.
    real: PathBuf,
    /// What it leads to, which is never a link.
    entry: Entry,
}

enum Step {
    Parent,
    Name(OsString),
}

/// The steps of a relative path, or nothing when it is not relative.
fn steps(path: &Path) -> Option<Vec<Step>> {
    let mut steps = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => steps.push(Step::Name(part.to_os_string())),
            Component::ParentDir => steps.push(Step::Parent),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(steps)
}

/// What a name in a folder is, seen without following a link.
enum Entry {
    Folder(Folder),
    File(fs::File),
    Link(PathBuf),
    /// A pipe, socket or device, which is never opened: a pipe would wait for a writer or take
    /// the data meant for the researcher's own reader.
    Other,
}

/// What `list_dir` shows of an entry.
enum Kind {
    Folder,
    /// A regular file, or a link with its own size.
    File(u64),
    Other,
}

/// A folder opened on the way along a path.
#[cfg(unix)]
struct Folder(std::os::fd::OwnedFd);

#[cfg(unix)]
impl Folder {
    fn open(path: &Path) -> io::Result<Self> {
        use rustix::fs::{Mode, OFlags};

        // Should a pipe or a link have taken the folder's place, neither wait for a writer
        // nor follow the link.
        let flags = OFlags::RDONLY
            | OFlags::DIRECTORY
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | OFlags::CLOEXEC;
        Ok(Self(rustix::fs::open(path, flags, Mode::empty())?))
    }

    fn try_clone(&self) -> io::Result<Self> {
        Ok(Self(self.0.try_clone()?))
    }

    /// Whether `path`, without following a link at its end, is this folder.
    fn is_at(&self, path: &Path) -> bool {
        let (Ok(opened), Ok(named)) = (rustix::fs::fstat(&self.0), rustix::fs::lstat(path)) else {
            return false;
        };
        (opened.st_dev, opened.st_ino) == (named.st_dev, named.st_ino)
    }

    fn entry(&self, name: &OsStr) -> io::Result<Entry> {
        use rustix::fs::{AtFlags, FileType, Mode, OFlags};
        use std::os::unix::ffi::OsStringExt;

        let stat = rustix::fs::statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW)?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Symlink => {
                let target = rustix::fs::readlinkat(&self.0, name, Vec::new())?;
                Ok(Entry::Link(OsString::from_vec(target.into_bytes()).into()))
            }
            FileType::Directory | FileType::RegularFile => {
                // Should a link or a pipe have taken its place since, neither follow the link
                // nor wait for a writer.
                let opened = rustix::fs::openat(
                    &self.0,
                    name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )?;
                Ok(
                    match FileType::from_raw_mode(rustix::fs::fstat(&opened)?.st_mode) {
                        FileType::Directory => Entry::Folder(Self(opened)),
                        FileType::RegularFile => Entry::File(opened.into()),
                        _ => Entry::Other,
                    },
                )
            }
            _ => Ok(Entry::Other),
        }
    }

    fn names(&self) -> io::Result<Vec<OsString>> {
        use std::os::unix::ffi::OsStrExt;

        let mut names = Vec::new();
        for entry in rustix::fs::Dir::read_from(&self.0)?.flatten() {
            let name = entry.file_name();
            if name != c"." && name != c".." {
                names.push(OsStr::from_bytes(name.to_bytes()).to_os_string());
            }
        }
        Ok(names)
    }

    fn list(&self) -> io::Result<Vec<(OsString, Option<Kind>)>> {
        use rustix::fs::{AtFlags, FileType};

        Ok(self
            .names()?
            .into_iter()
            .map(|name| {
                let kind = rustix::fs::statat(&self.0, &name, AtFlags::SYMLINK_NOFOLLOW)
                    .ok()
                    .map(|stat| match FileType::from_raw_mode(stat.st_mode) {
                        FileType::Directory => Kind::Folder,
                        FileType::RegularFile | FileType::Symlink => {
                            Kind::File(stat.st_size.try_into().unwrap_or_default())
                        }
                        _ => Kind::Other,
                    });
                (name, kind)
            })
            .collect())
    }
}

/// A folder on the way along a path. The standard library cannot open a name inside an opened
/// folder here, so a link that takes a folder's place after it was looked at is followed.
#[cfg(not(unix))]
struct Folder(PathBuf);

#[cfg(not(unix))]
impl Folder {
    fn open(path: &Path) -> io::Result<Self> {
        Ok(Self(path.to_path_buf()))
    }

    fn try_clone(&self) -> io::Result<Self> {
        Ok(Self(self.0.clone()))
    }

    /// Whether `path` leads to this folder without a link.
    fn is_at(&self, path: &Path) -> bool {
        path.canonicalize()
            .is_ok_and(|canonical| canonical == self.0)
    }

    fn names(&self) -> io::Result<Vec<OsString>> {
        Ok(fs::read_dir(&self.0)?
            .flatten()
            .map(|entry| entry.file_name())
            .collect())
    }

    fn entry(&self, name: &OsStr) -> io::Result<Entry> {
        let path = self.0.join(name);
        let metadata = fs::symlink_metadata(&path)?;
        Ok(if metadata.is_symlink() {
            Entry::Link(fs::read_link(&path)?)
        } else if metadata.is_dir() {
            Entry::Folder(Self(path))
        } else if metadata.is_file() {
            Entry::File(fs::File::open(&path)?)
        } else {
            Entry::Other
        })
    }

    fn list(&self) -> io::Result<Vec<(OsString, Option<Kind>)>> {
        Ok(fs::read_dir(&self.0)?
            .flatten()
            .map(|entry| {
                let kind = entry.metadata().ok().map(|metadata| {
                    if metadata.is_dir() {
                        Kind::Folder
                    } else if metadata.is_file() || metadata.is_symlink() {
                        Kind::File(metadata.len())
                    } else {
                        Kind::Other
                    }
                });
                (entry.file_name(), kind)
            })
            .collect())
    }
}

fn read_file(path: &str, entry: Entry) -> Result<String, String> {
    let file = match entry {
        Entry::File(file) => file,
        Entry::Folder(_) => return Err(format!("{path} is a folder")),
        Entry::Link(_) | Entry::Other => {
            return Err(format!(
                "{path} is not a regular file, such as a pipe or a device"
            ));
        }
    };
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
        let tools = project_in(dir.path());
        (dir, tools)
    }

    fn project_in(root: &Path) -> Tools {
        for (path, contents) in [
            ("data/a.csv", "id,name\n1,x\n"),
            (".fair-witness/credentials.toml", "secret"),
            (".fair-witness/ignore", "scratch/\nprivate.csv\n"),
            // As `fair-witness setup` in `data/` leaves it.
            ("data/.fair-witness/credentials.toml", "secret"),
            (".git/config", "url = https://token@example.org/lab.git\n"),
            ("scratch/tmp.csv", "id\n"),
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        Tools::new(root, Filter::new(root).unwrap()).unwrap()
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
    fn reads_names_only_as_they_are_spelled() {
        let (_dir, tools) = project();
        // A file system that ignores case, as macOS and Windows do by default, finds
        // `.fair-witness/` and `.git/` under these names, which the filter does not know.
        for path in [
            ".FAIR-WITNESS/credentials.toml",
            ".Git/config",
            "DATA/A.CSV",
        ] {
            for tool in ["read_file", "list_dir"] {
                assert!(call(&tools, tool, path).is_err(), "{tool} {path}");
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn judges_a_path_through_a_link_by_its_name_and_where_it_leads() {
        use std::os::unix::fs::symlink;

        let (dir, _) = project();
        let root = dir.path();
        fs::write(
            root.join(".fair-witness/ignore"),
            "results/a.csv\nnotes/a.csv\n",
        )
        .unwrap();
        fs::write(root.join("data/b.csv"), "id\n").unwrap();
        fs::create_dir_all(root.join("data/raw")).unwrap();
        fs::create_dir(root.join("notes")).unwrap();
        symlink("data", root.join("results")).unwrap();
        symlink("../data/raw", root.join("notes/raw")).unwrap();
        let tools = Tools::new(root, Filter::new(root).unwrap()).unwrap();

        // The rule for the name holds in the folder the link leads to as for the file.
        assert_eq!(
            call(&tools, "read_file", "results/a.csv"),
            Err("results/a.csv is not part of the research data".to_string())
        );
        assert_eq!(
            call(&tools, "list_dir", "results").unwrap(),
            "b.csv (3 bytes)\nraw/"
        );
        // `..` leaves the folder the link leads to, `data/raw/`, as the system does, so this is
        // `data/a.csv` and not `notes/a.csv`.
        assert_eq!(
            call(&tools, "read_file", "notes/raw/../a.csv").unwrap(),
            "id,name\n1,x\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn stops_at_a_loop_of_links() {
        let (dir, tools) = project();
        std::os::unix::fs::symlink("loop", dir.path().join("loop")).unwrap();
        assert_eq!(
            call(&tools, "read_file", "loop"),
            Err("loop: too many links".to_string())
        );
    }

    #[test]
    #[cfg(unix)]
    fn refuses_a_pipe_without_waiting_for_a_writer() {
        let (dir, tools) = project();
        let stream = dir.path().join("data/stream");
        let status = std::process::Command::new("mkfifo")
            .arg(&stream)
            .status()
            .unwrap();
        assert!(status.success());

        assert_eq!(
            call(&tools, "list_dir", "data").unwrap(),
            "a.csv (12 bytes)\nstream (not a regular file)"
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        let filter = Filter::new(dir.path()).unwrap();
        let root = stream.clone();
        std::thread::spawn(move || {
            let read = call(&tools, "read_file", "data/stream");
            // As if a pipe had taken the project folder's place.
            let made = Tools::new(&root, filter)
                .map(|_| ())
                .map_err(|e| e.to_string());
            sender.send((read, made))
        });
        let (read, made) = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert_eq!(
            read,
            Err("data/stream is not a regular file, such as a pipe or a device".to_string())
        );
        let canonical = stream.canonicalize().unwrap();
        assert_eq!(made, Err(format!("could not open {}", canonical.display())));
    }

    #[test]
    #[cfg(unix)]
    fn keeps_to_what_it_checked_when_links_take_its_place() {
        use std::os::unix::fs::symlink;

        let (dir, tools) = project();
        let root = dir.path();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        let file = tools.find("data/a.csv").unwrap();
        let folder = tools.find("data").unwrap();

        // Between the check and the read, another process puts a link out of the project in
        // the file's place, then a link into `.fair-witness/` in its folder's.
        fs::rename(root.join("data/a.csv"), root.join("data/b.csv")).unwrap();
        symlink(outside.path().join("secret.txt"), root.join("data/a.csv")).unwrap();
        fs::rename(root.join("data"), root.join("old")).unwrap();
        symlink(".fair-witness", root.join("data")).unwrap();

        assert_eq!(
            read_file("data/a.csv", file.entry).unwrap(),
            "id,name\n1,x\n"
        );
        let link_size = outside.path().join("secret.txt").as_os_str().len();
        assert_eq!(
            tools.list_dir("data", folder).unwrap(),
            format!("a.csv ({link_size} bytes)\nb.csv (12 bytes)")
        );
    }

    #[test]
    #[cfg(unix)]
    fn stops_when_the_project_folder_is_replaced() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("project");
        let tools = project_in(&root);
        fs::rename(&root, parent.path().join("saved")).unwrap();

        let replaced = Err(
            "data/a.csv: the project folder was moved or replaced since the review started"
                .to_string(),
        );
        std::os::unix::fs::symlink("saved/.fair-witness", &root).unwrap();
        assert_eq!(call(&tools, "read_file", "data/a.csv"), replaced);
        fs::remove_file(&root).unwrap();
        // An empty folder in its place has no cache tags, which the filter looks for by path.
        fs::create_dir(&root).unwrap();
        assert_eq!(call(&tools, "read_file", "data/a.csv"), replaced);
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
