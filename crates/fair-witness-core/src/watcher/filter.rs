use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::project::{DIR_NAME, IGNORE_FILE};

/// Folders tools fill on their own and files editors and file managers leave
/// behind. The project's `.gitignore` is deliberately not read: data kept out
/// of git is often exactly the data the DMP is about.
const BUILT_IN: &[&str] = &[
    ".git",
    ".venv/",
    "venv/",
    "__pycache__/",
    "node_modules/",
    ".ipynb_checkpoints/",
    "*.swp",
    "*.swo",
    "*.swx",
    "*~",
    ".#*",
    // Escaped, because a line starting with `#` is a comment.
    r"\#*#",
    ".DS_Store",
];

/// The file that marks a folder as a cache, as the Cache Directory Tagging
/// Specification (https://bford.info/cachedir/) describes.
pub const CACHE_TAG: &str = "CACHEDIR.TAG";
/// A tag only counts when it starts with this, so that a file that happens to
/// have the name does not hide a folder.
const CACHE_TAG_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// Decides which paths in the project folder are not research data.
pub struct Filter {
    root: PathBuf,
    rules: Gitignore,
}

impl Filter {
    /// Reads the built-in rules, then `.fair-witness/ignore`, which can add
    /// rules or undo built-in ones with `!`, including the one for folders
    /// tagged as caches.
    pub fn new(root: &Path) -> Result<Self> {
        let mut builder = GitignoreBuilder::new(root);
        for rule in BUILT_IN {
            builder.add_line(None, rule)?;
        }
        let file = root.join(DIR_NAME).join(IGNORE_FILE);
        if file.is_file()
            && let Some(e) = builder.add(&file)
        {
            // The error names the file and the line.
            return Err(e).context("an ignore rule is invalid");
        }
        let rules = builder
            .build()
            .context("could not build the ignore rules")?;
        Ok(Self {
            root: root.to_path_buf(),
            rules,
        })
    }

    pub fn skips(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        if relative.as_os_str().is_empty() {
            return false;
        }
        // fair-witness writes its own files there, so no rule can let it
        // through.
        if relative.starts_with(DIR_NAME) {
            return true;
        }
        // As in git, a file in an ignored folder stays ignored even if a rule
        // re-includes the file, which `matched_path_or_any_parents` allows.
        let mut folders: Vec<&Path> = relative.ancestors().skip(1).collect();
        folders.pop();
        folders.iter().rev().any(|folder| self.skips_folder(folder))
            || if is_dir {
                self.skips_folder(relative)
            } else {
                self.rules.matched(relative, false).is_ignore()
            }
    }

    fn skips_folder(&self, relative: &Path) -> bool {
        match self.rules.matched(relative, true) {
            Match::Ignore(_) => true,
            Match::Whitelist(_) => false,
            // Read on every call, because a tool can tag a folder after
            // creating it, as cargo does with `target/`.
            Match::None => is_cache(&self.root.join(relative)),
        }
    }
}

fn is_cache(folder: &Path) -> bool {
    let path = folder.join(CACHE_TAG);
    // The specification allows only a regular file, not a link. Opening a
    // named pipe would also wait for a writer and stop the watcher.
    if !fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let Ok(tag) = File::open(path) else {
        return false;
    };
    let mut start = Vec::with_capacity(CACHE_TAG_SIGNATURE.len());
    tag.take(CACHE_TAG_SIGNATURE.len() as u64)
        .read_to_end(&mut start)
        .is_ok()
        && start == CACHE_TAG_SIGNATURE
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn filter(rules: &str) -> (tempfile::TempDir, Result<Filter>) {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(DIR_NAME)).unwrap();
        fs::write(root.path().join(DIR_NAME).join(IGNORE_FILE), rules).unwrap();
        let filter = Filter::new(root.path());
        (root, filter)
    }

    #[test]
    fn the_setup_template_adds_no_ignore_rules() {
        let root = tempfile::tempdir().unwrap();
        crate::project::Project::new(root.path().to_path_buf())
            .create()
            .unwrap();

        let filter = Filter::new(root.path()).unwrap();

        assert_eq!(filter.rules.num_ignores(), BUILT_IN.len() as u64);
        assert_eq!(filter.rules.num_whitelists(), 0);
    }

    #[test]
    fn reads_rules_like_git() {
        let (root, filter) = filter("!venv/\n!.fair-witness/\nscratch/\n!scratch/keep.csv\n");
        let filter = filter.unwrap();
        let skipped = |path: &str| filter.skips(&root.path().join(path), false);

        assert!(skipped(".git/objects/ab/cdef"));
        assert!(skipped("#notes.txt#"));
        assert!(!skipped("venv/answers.csv"));
        assert!(skipped(".fair-witness/credentials.toml"));
        assert!(skipped("scratch/keep.csv"));
    }

    #[test]
    fn skips_folders_tagged_as_caches_unless_a_rule_keeps_them() {
        let (root, filter) = filter("!kept/\n");
        let filter = filter.unwrap();
        for (folder, tag) in [
            (
                "target",
                "Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n",
            ),
            ("kept", "Signature: 8a477f597d28d172789f06886806bc55"),
            ("notes", "Signature: of the participants, scanned\n"),
        ] {
            fs::create_dir(root.path().join(folder)).unwrap();
            fs::write(root.path().join(folder).join(CACHE_TAG), tag).unwrap();
        }
        let skipped = |path: &str, is_dir| filter.skips(&root.path().join(path), is_dir);

        assert!(skipped("target", true));
        assert!(skipped("target/debug/app", false));
        assert!(!skipped("kept/model.bin", false));
        assert!(!skipped("notes", true));
        assert!(!skipped("notes/CACHEDIR.TAG", false));
    }

    #[cfg(unix)]
    #[test]
    fn trusts_only_a_tag_that_is_a_regular_file() {
        let (root, filter) = filter("");
        let filter = filter.unwrap();
        let root = root.path();
        for folder in ["cache", "linked", "piped"] {
            fs::create_dir(root.join(folder)).unwrap();
        }
        fs::write(
            root.join("cache").join(CACHE_TAG),
            "Signature: 8a477f597d28d172789f06886806bc55",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            root.join("cache").join(CACHE_TAG),
            root.join("linked").join(CACHE_TAG),
        )
        .unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(root.join("piped").join(CACHE_TAG))
            .status()
            .unwrap();
        assert!(status.success());

        assert!(filter.skips(&root.join("cache/a.csv"), false));
        assert!(!filter.skips(&root.join("linked/a.csv"), false));
        assert!(!filter.skips(&root.join("piped/a.csv"), false));
    }

    #[test]
    fn names_the_ignore_file_when_a_rule_is_invalid() {
        let (root, filter) = filter("data/[z-a].csv\n");
        let message = format!("{:#}", filter.err().unwrap());

        let file = root.path().join(".fair-witness/ignore");
        assert_eq!(
            message,
            format!(
                "an ignore rule is invalid: {}: line 1: \
                 error parsing glob 'data/[z-a].csv': invalid range; 'z' > 'a'",
                file.display()
            )
        );
    }
}
