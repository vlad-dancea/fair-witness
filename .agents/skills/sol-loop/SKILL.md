---
name: sol-loop
description: Use when the user says "sol loop" or "soul loop", or when full-stop or full-send reach their review step. Has GPT-6.1-Sol review the current change in rounds until no serious finding is left.
---

# Sol loop

A second model reads the change with fresh eyes and tries to break it. You fix what it finds or
push back with evidence. It works before a pull request exists or on one that is already open.
It needs T3 Code's `delegate_task` tool. Without it, say so and stop.

## Each round

1. Run `git fetch origin`. Collect the change: `git diff origin/main...HEAD` for commits,
   `git diff HEAD` for staged and unstaged work, and `git status --short -uall` for new files,
   which no diff shows.
2. Call `orchestrator_capabilities`, then `delegate_task` with provider instance `codex`, model
   `gpt-6.1-sol`, option `reasoningEffort: high`, role `review`, interaction mode `plan` (so it
   cannot edit) and mode `async`. End your turn and let its completion wake you.
3. The reviewer sees only your task text, so the brief stands alone:
   - the goal of the change in a few sentences and the worktree path
   - read `AGENTS.md`, then the diff and every new file by path
   - find what is wrong: correctness, data loss, security, leaks of the login or API key, files
     the model may read that it should not, missing tests, broken rules from `AGENTS.md`
   - every finding gives file:line, a concrete failure (this input gives that wrong result) and
     a severity of high, medium or low
   - no style nits, no subagents, and "NONE" when nothing is wrong
   - from round 2 on: every earlier finding, your response to it and the open objections. The
     reviewer accepts each rebuttal or holds its position with new evidence.
4. Verify every finding against the source before acting. Fix the correct ones. Rebut the rest
   with evidence: a test, a run against TU Wien's DAMAP, or the quoted code. Never agree just to
   end the loop.
5. Run `mise run check` after fixing.

Each round is a new `delegate_task` call with its own `clientRequestId`. Never send follow-ups to
the reviewer's child thread. Run one reviewer at a time.

## When to stop

Stop when the reviewer answers "NONE" or only low findings remain open. The budget is 3 rounds.
Allow up to 5 only while an open finding concerns security, permissions, secrets, releases or
data loss.

A finding that needs a design decision goes to the maintainer instead of another round.

## Report

List every finding in a table with its round, severity, file:line and outcome: fixed, rebutted
and accepted, or still disputed. Disputed findings are the maintainer's call. The loop itself
never commits or pushes. Full stop and full send do that.
