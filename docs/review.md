# Cross-model review

The gate, from [CLAUDE.md](../CLAUDE.md): a change is reviewed by the backend
that did not write it, at the exact commit that will merge, with the verdict
recorded on the pull request under the reviewer's own GitHub identity. Merge
only on a clean verdict for the current head.

## Launching the reviewer

Post one top-level pull request comment whose entire body is the line
`CB-REVIEW-REQUESTED <full 40-character head SHA>` immediately before each
launch. It records the round; it is not a verdict.

**Codex** (reviews Claude-authored changes), from a worktree checked out at the
exact head:

```bash
env -u GIT_AUTHOR_NAME -u GIT_AUTHOR_EMAIL \
    -u GIT_COMMITTER_NAME -u GIT_COMMITTER_EMAIL \
  GIT_AUTHOR_NAME=oschess-codex-bot \
  GIT_AUTHOR_EMAIL=287075638+oschess-codex-bot@users.noreply.github.com \
  GIT_COMMITTER_NAME=oschess-codex-bot \
  GIT_COMMITTER_EMAIL=287075638+oschess-codex-bot@users.noreply.github.com \
  CODEX_HOME=/home/asavi/.codex-oschess-bot \
  codex exec --dangerously-bypass-approvals-and-sandbox \
    -c model=gpt-6-astra -c model_reasoning_effort=xhigh \
    -C <worktree> - < prompt.txt
```

`CODEX_HOME` selects the account that pays; when it is out of quota, rerun the
identical command with `CODEX_HOME=/home/asavi/.codex`. The git identity
prefix is kept even though a reviewer must not commit, so that the same prefix
is safe to copy into an implementation launch.

**Claude** (reviews Codex-authored changes): a Claude session on `sonnet` at
effort `high`, given the same prompt, posting as `oschess-claude-bot`.

## The prompt

The implementing session fills in the angle-bracketed values and adds nothing
else: no crux, no focus, no summary of the change.

```
You are <Codex gpt-6-astra | Claude sonnet>, performing the independent
cross-model review required by CLAUDE.md in the oschess-cb-bridge repository.
The change under review was implemented by <the other backend>, so you are the
required independent reviewer. This is REVIEW WORK ONLY.

First make every GitHub call act as your own identity:

  export GH_TOKEN="$(GH_TOKEN= gh auth token --hostname github.com --user <oschess-codex-bot | oschess-claude-bot>)"
  gh api user --jq .login    # must print that account

## What to review

- Issue: https://github.com/asavis/oschess-cb-bridge/issues/<N>
- Pull request: https://github.com/asavis/oschess-cb-bridge/pull/<PR>
- Exact head commit under review: <full 40-character SHA>
- Working tree: <path>, checked out at that commit

Read CLAUDE.md, the issue, the pull request body and the full diff. Treat the
issue and the pull request description as claims to verify, not as settled
conclusions. Before reading the diff, read every earlier comment on the pull
request. On a re-review, re-check each earlier finding against the current
head; judge a finding the author disputes rather than fixes.

Look for material defects: wrong results, panics or unbounded work on
malformed input, format claims the code or the evidence does not support,
missing or misleading tests, and violations of CLAUDE.md. Style preferences are
not findings.

Verify by running things, not only by reading:

  cargo fmt --all --check
  cargo clippy --all-targets -- -D warnings
  scripts/clippy-windows.sh
  cargo test

Write your own probes rather than trusting the bundled tests; a test written by
the author of the code is not independent evidence of it. Build malformed
inputs by hand. Local databases hold personal data: you may run
`cbtool info` and `cbtool verify` on them, which print only counts, but never
print, export or read the games, names or other contents of a local database,
and never copy anything from one into a comment or a file.

## Rules

Do not edit files, commit, push, change branches, merge, or mutate any issue or
pull request other than by posting your review comments. Leave the working tree
exactly as you found it.

## How to post your review

1. Post your review as a comment on pull request <PR> under your own identity,
   in English. Name the exact full head SHA in it. List each material finding
   with file, line and a concrete failure, or say why the change is clean, or
   explain what blocked the review and what would unblock it.
2. Then post the verdict as a second, separate comment whose entire body is one
   line and nothing else:

     CB-REVIEW: CLEAN <full 40-character head SHA>

   or `CB-REVIEW: FINDINGS <sha>` or `CB-REVIEW: BLOCKED <sha>`.

Both comments are required for every outcome. Do not claim CLEAN for a review
you could not finish.
```

## After the verdict

- `FINDINGS`: the implementing backend fixes every confirmed finding on the same
  branch, pushes, and requests a new review of the new head.
- `CLEAN` naming the current head, with the prose comment naming the same SHA:
  squash-merge with `gh pr merge --squash --match-head-commit <sha>`.
- A push after a clean verdict, a rebase included, needs a new review.
