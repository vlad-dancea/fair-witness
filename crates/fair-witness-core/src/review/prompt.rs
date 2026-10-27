use std::fs;
use std::path::Path;

use crate::watcher::{self, Change, Kind, Listed};

/// How the model should judge, kept in a file of its own to be read and diffed as text.
pub const SYSTEM_PROMPT: &str = include_str!("prompt.md").trim_ascii_end();

/// The first message of a review: the DMP, then `about_changes`.
pub fn first_message(dmp: &str, about_changes: &str) -> String {
    format!("The DMP:\n```json\n{dmp}\n```\n\n{about_changes}")
}

/// What changed in the folder under `root` and what earlier steps of the review found, which
/// the first message gives after the DMP.
pub fn about_changes(root: &Path, changes: &[Change], found: &[String]) -> String {
    let listed = watcher::summarize(changes);
    let lines: Vec<String> = listed
        .iter()
        .map(|listed| {
            let line = format!("{}: {}", listed.kind().verb(), listed.describe());
            match listed {
                // A set waits for the reviews before it, and its files can go meanwhile.
                Listed::One(change)
                    if change.kind() != Kind::Deleted
                        && fs::symlink_metadata(root.join(change.path())).is_err() =>
                {
                    format!("{line} (gone since, so it can no longer be read)")
                }
                _ => line,
            }
        })
        .collect();
    let mut prompt = format!("What changed in the project folder:\n{}", lines.join("\n"));
    if listed
        .iter()
        .any(|listed| matches!(listed, Listed::Many { .. }))
    {
        prompt.push_str(
            "\n\nThere were too many changes to name each path. list_dir shows the \
             paths that still exist; deleted ones are gone.",
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
            about_changes(root.path(), &changes, &[]),
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
            about_changes(root.path(), &changes, &[]),
            "What changed in the project folder:\n\
             changed: notes.txt\n\
             changed: results.csv (gone since, so it can no longer be read)\n\
             created: data (gone since, so it can no longer be read)\n\
             renamed: draft.csv -> final.csv (gone since, so it can no longer be read)\n\
             deleted: old.csv"
        );
    }
}
