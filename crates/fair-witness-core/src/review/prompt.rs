use crate::watcher::{self, Change, Listed};

/// How the model should judge, kept in a file of its own to be read and diffed as text.
pub const SYSTEM_PROMPT: &str = include_str!("prompt.md").trim_ascii_end();

/// The first message of a review: the DMP, what changed in the folder and what earlier steps of
/// the review found.
pub fn user_message(dmp: &str, changes: &[Change], found: &[String]) -> String {
    let listed = watcher::summarize(changes);
    let lines: Vec<String> = listed
        .iter()
        .map(|listed| format!("{}: {}", listed.kind().verb(), listed.describe()))
        .collect();
    let mut prompt = format!(
        "The DMP:\n```json\n{dmp}\n```\n\nWhat changed in the project folder:\n{}",
        lines.join("\n")
    );
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
