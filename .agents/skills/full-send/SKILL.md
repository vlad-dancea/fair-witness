---
name: full-send
description: Use only when the user says "full send". Does everything full-stop does, then squash-merges the pull request and watches CI and any release it publishes.
---

# Full send

Saying "full send" is the maintainer's permission to commit, push, open the pull request and merge
it. A merge to `main` that changes what gets built publishes a release to everyone installing
fair-witness, so merge only when every gate below holds. When one cannot be met, stop and report
as full stop does.

## Steps

1. Run the `full-stop` skill through to its ready state.
2. Read the pull request head with `gh pr view <number> --json headRefOid` and record it as
   `<validated-sha>`. Check the merge gates once more for that exact commit:
   - every gate from full stop holds
   - no finding is still disputed and nothing waits for a maintainer decision
   - nobody requested changes in a review
   - the pull request's Verification section covers every change
   - no other open pull request targets this branch, or it was pointed at `main` first
3. Squash-merge as `AGENTS.md` describes, with the subject `<title> (#<number>)` and a body that
   follows the commit message rules. Pass `--match-head-commit <validated-sha>`. If the head
   changed, repeat step 2 before trying again. Never force-push and never enable auto-merge.
4. Read the squash commit SHA with `gh pr view <number> --json mergeCommit`, waiting until
   `mergeCommit` is non-null. Use its `oid` as `<merge-sha>`. Wait for the `ci` run on `main`
   for that commit to appear with `gh run list --branch main --commit <merge-sha>`, then watch
   it until it finishes with `gh run watch <id>`.
5. If its `publish` job ran, find the release run it started with
   `gh run list --workflow release.yml --event workflow_dispatch`, watch it with
   `gh run watch <id>`, then confirm with `gh release view <tag>` that the release is the latest
   one and lists the installers.
6. If the check, the publish job or the release fails, report at once with the run link and the
   failing step, and propose a fix or revert pull request. Do not push to `main`, re-run a failed
   release or delete a tag without the maintainer: re-running an older release makes it the
   latest one, so installers would serve it.

## Report

- the merged pull request and its squash commit
- the CI result on `main`, and the release with its version, or that the merge released nothing
- anything that needs the maintainer, such as a failed release
