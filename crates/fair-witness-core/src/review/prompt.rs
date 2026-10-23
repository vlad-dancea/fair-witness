use crate::watcher::{self, Change, Listed};

pub const SYSTEM_PROMPT: &str = "\
You check a researcher's project folder against their data management plan (DMP).
You get the DMP as maDMP JSON (RDA DMP Common Standard) and a list of files that
just changed in the folder. Decide whether the changes contradict the DMP: for
example data of a kind, format, size or sensitivity the DMP does not describe,
personal data where the DMP says there is none, or files in a format the DMP
rules out. Changes the DMP does not cover at all (notes, scripts, drafts) do not
contradict it.

Use list_dir and read_file to look at the changed files when their names alone
do not settle it. Be frugal: read only what you need. Then call report_verdict
exactly once. Only report a contradiction you can point to in the DMP.";

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
