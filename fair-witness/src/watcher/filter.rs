use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::config::{DIR_NAME, IGNORE_FILE};

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

/// Decides which paths in the project folder are not research data.
pub struct Filter {
    root: PathBuf,
    rules: Gitignore,
}

impl Filter {
    /// Reads the built-in rules, then `.fair-witness/ignore`, which can add
    /// rules or undo built-in ones with `!`.
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
        folders
            .iter()
            .rev()
            .any(|folder| self.rules.matched(folder, true).is_ignore())
            || self.rules.matched(relative, is_dir).is_ignore()
    }
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
