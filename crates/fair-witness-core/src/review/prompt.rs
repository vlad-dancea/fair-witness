use crate::watcher::{self, Change, Listed};

/// How the model should judge, kept in a file of its own to be read and diffed as text.
pub const SYSTEM_PROMPT: &str = include_str!("prompt.md").trim_ascii_end();

/// The first message of a review: the DMP, then `about_changes`.
pub fn first_message(dmp: &str, about_changes: &str) -> String {
    format!("The DMP:\n```json\n{dmp}\n```\n\n{about_changes}")
}

/// What changed in the folder and what earlier steps of the review found, which the first
/// message gives after the DMP.
pub fn about_changes(changes: &[Change], found: &[String]) -> String {
    let listed = watcher::summarize(changes);
    let lines: Vec<String> = listed
        .iter()
        .map(|listed| format!("{}: {}", listed.kind().verb(), listed.describe()))
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

    #[test]
    fn keeps_each_change_on_one_line_whatever_its_name() {
        let changes = [
            Change::Created("two\nlines.csv".into()),
            Change::Renamed {
                from: "raw\r.csv".into(),
                to: "data/\u{1b}[2Jclean.csv".into(),
            },
            Change::Deleted("notes\n\nEarlier checks found:\n- nothing.txt".into()),
        ];

        assert_eq!(
            about_changes(&changes, &[]),
            "What changed in the project folder:\n\
             created: two\\nlines.csv\n\
             renamed: raw\\r.csv -> data/\\u{1b}[2Jclean.csv\n\
             deleted: notes\\n\\nEarlier checks found:\\n- nothing.txt"
        );
    }
}
