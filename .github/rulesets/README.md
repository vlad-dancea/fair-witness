# Main branch protection

`main.json` is the "Protect main" ruleset as GitHub returns it. GitHub applies the ruleset
through its API, so committing a change to this file changes nothing until someone applies it.
The active ruleset is under Settings, Rules, Rulesets.

Every change to `main` needs a pull request that merges by squash, once the `check` job from
the GitHub Actions app passes on a branch that contains the latest `main` and every review
thread is resolved. Deletion, force pushes and non-linear history are blocked, and nobody can
bypass the ruleset, the repository owner included.

No approving review is required, because the maintainer works alone and GitHub does not let
the author of a pull request approve it. When a second reviewer joins, set
`required_approving_review_count` to `1` and `require_last_push_approval` to `true`. Stale
approvals are already dismissed. From then on, a pull request that Copilot opens without a person
behind it needs one approval more, because `require_extra_approval_for_unattributed_changes` is
on. With no approvals required, that setting has no effect. CodeRabbit is not a required check, but its review threads
have to be resolved like any other.

`check` is the only required check. dist's `plan` job runs on every pull request too, but it is
not required. The `publish` job pushes `v*` tags, which this branch ruleset does not cover.

Some settings sit outside the ruleset, in the repository settings: merge commits and rebase
merges are off, the default squash title is the pull request title, the "Update branch" button
is on, and GitHub deletes a branch once its pull request merges. Secret scanning and push
protection are on.

To apply a reviewed change, find the ruleset's ID, replace the ruleset with the file and read
it back:

```sh
gh api repos/vlad-dancea/fair-witness/rulesets --jq '.[] | {id, name}'
gh api --method PUT repos/vlad-dancea/fair-witness/rulesets/RULESET_ID \
  --input .github/rulesets/main.json
gh api repos/vlad-dancea/fair-witness/rulesets/RULESET_ID
```

A mistake here can lock everyone out of `main`, but an administrator can still edit the
ruleset to recover.
