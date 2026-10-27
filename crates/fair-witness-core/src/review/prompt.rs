use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::watcher::{self, Change, Kind, Listed};

/// How the model should judge, kept in a file of its own to be read and diffed as text.
pub const SYSTEM_PROMPT: &str = include_str!("prompt.md").trim_ascii_end();

/// The first message of a review: the DMP, then `about_changes`.
pub fn first_message(dmp: &str, about_changes: &str) -> String {
    format!("The DMP:\n```json\n{dmp}\n```\n\n{about_changes}")
}

/// The paths of `changes` under `root` that are gone by the time of the review. A set waits for
/// the reviews before it, and its files can go meanwhile.
pub fn gone(root: &Path, changes: &[Change]) -> HashSet<PathBuf> {
    changes
        .iter()
        .filter(|change| change.kind() != Kind::Deleted)
        .map(Change::path)
        .filter(|path| fs::symlink_metadata(root.join(path)).is_err())
        .map(Path::to_path_buf)
        .collect()
}

/// `change` as the model is told it, in the first message or by `list_changes`.
pub fn line(change: &Change, gone: &HashSet<PathBuf>) -> String {
    let line = format!(
        "{}: {}",
        change.kind().verb(),
        Listed::One(change).describe()
    );
    if change.kind() != Kind::Deleted && gone.contains(change.path()) {
        format!("{line} (gone since, so it can no longer be read)")
    } else {
        line
    }
}

/// What changed, with the paths that are `gone`, and what earlier steps of the review found,
/// which the first message gives after the DMP.
pub fn about_changes(changes: &[Change], gone: &HashSet<PathBuf>, found: &[String]) -> String {
    let listed = watcher::summarize(changes);
    let lines: Vec<String> = listed
        .iter()
        .map(|listed| match listed {
            Listed::One(change) => line(change, gone),
            many => format!("{}: {}", many.kind().verb(), many.describe()),
        })
        .collect();
    let mut prompt = format!("What changed in the project folder:\n{}", lines.join("\n"));
    if listed
        .iter()
        .any(|listed| matches!(listed, Listed::Many { .. }))
    {
        prompt.push_str(
            "\n\nThere were too many changes to name each path here. list_changes names \
             each of them, a page at a time.",
        );
    }
    if !found.is_empty() {
        prompt.push_str("\n\nEarlier checks found:");
        for finding in found {
            prompt.push_str(&format!("\n- {finding}"));
        }
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    // Such names exist only where a name can hold control characters.
    #[cfg(unix)]
    #[test]
    fn keeps_each_change_on_one_line_whatever_its_name() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("data")).unwrap();
        for path in ["two\nlines.csv", "data/\u{1b}[2Jclean.csv"] {
            fs::write(root.path().join(path), "x").unwrap();
        }
        let changes = [
            Change::Created("two\nlines.csv".into()),
            Change::Renamed {
                from: "raw\r.csv".into(),
                to: "data/\u{1b}[2Jclean.csv".into(),
            },
            Change::Deleted("notes\n\nEarlier checks found:\n- nothing.txt".into()),
        ];

        assert_eq!(
            about_changes(&changes, &gone(root.path(), &changes), &[]),
            "What changed in the project folder:\n\
             created: two\\nlines.csv\n\
             renamed: raw\\r.csv -> data/\\u{1b}[2Jclean.csv\n\
             deleted: notes\\n\\nEarlier checks found:\\n- nothing.txt"
        );
    }

    #[test]
    fn says_which_files_are_gone_by_the_time_of_the_review() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("notes.txt"), "x").unwrap();
        let changes = [
            Change::Changed("notes.txt".into()),
            Change::Changed("results.csv".into()),
            Change::Created("data".into()),
            Change::Renamed {
                from: "draft.csv".into(),
                to: "final.csv".into(),
            },
            Change::Deleted("old.csv".into()),
        ];

        assert_eq!(
            about_changes(&changes, &gone(root.path(), &changes), &[]),
            "What changed in the project folder:\n\
             changed: notes.txt\n\
             changed: results.csv (gone since, so it can no longer be read)\n\
             created: data (gone since, so it can no longer be read)\n\
             renamed: draft.csv -> final.csv (gone since, so it can no longer be read)\n\
             deleted: old.csv"
        );
    }
}
