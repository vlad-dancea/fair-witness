---
name: full-stop
description: Use when the user says "full stop". Takes the current work to a pull request that is ready to merge, babysits it through CI and CodeRabbit, and never merges.
---

# Full stop

Carry the work to a green, reviewed pull request without the maintainer having to watch. Saying
"full stop" is the maintainer's permission to commit, push and open the pull request. It is not
permission to merge.

Ask the maintainer only when you cannot go on alone: a design decision, a finding you doubt, or a
missing permission. Use the harness's question tool, ask once with everything you need, then keep
going. Everything else follows `AGENTS.md`, which wins over this skill when the two disagree.

## Steps

1. Finish the work and prove it. Run `mise run check` and the verification `AGENTS.md` asks for,
   such as a run against TU Wien's DAMAP for changes to `damap/` or `auth.rs`.
2. Run the `sol-loop` skill on the change. Stop here and ask if a high finding stays disputed.
3. Commit in small commits that each pass `mise run check`, push, and open the pull request
   against `main` as described in `AGENTS.md`. Never open a draft, because CodeRabbit skips
   drafts. If a pull request already exists for the branch, use it, and mark a draft ready with
   `gh pr ready <number>`.
4. Link the pull request to the thread with `link_pull_request`. If it already existed, check
   the whole pull request as in step 5 first, including the bodies of reviews already posted,
   because only new comments wake you. Then call `watch_pull_request` and end your turn. T3 Code
   wakes you when a check fails, the checks pass, someone comments or reviews, or the branch
   conflicts. Do not poll.
5. On each wake, check the whole pull request, not only the event that woke you:
   - A failed check: find why it differs from the local check before retrying. Retry only a
     failure you can show is infrastructure, not code.
   - CodeRabbit and other review comments, including findings in the body of a review: handle
     them as `AGENTS.md` describes. Push the fixes and wait for the next review.
   - `main` moved: merge `origin/main` into the branch, run `mise run check` and push.
6. The pull request is ready when:
   - the `check` job and dist's `plan` job pass on the latest commit
   - `gh pr checks <number>` shows the `CodeRabbit` check passing with "Review completed" for the
     latest commit
   - every review thread is resolved, or answered with a reason and left for the maintainer
   - every finding in the body of a review is fixed, or answered with a reason in a comment
   - the branch contains the latest `main`
   - for a stacked pull request, the description says which one merges first
7. Call `unwatch_pull_request` and report.

## Report

Write the report so it can be read in one pass:

- the pull request link
- one line on what changed and how it was verified
- whether merging it publishes a release, by the paths in `AGENTS.md`
- the sol loop and CodeRabbit findings with what happened to each, as a short table
- what is left for the maintainer to decide, if anything
- the next step: "Ready to merge. Say full send or merge it yourself."
