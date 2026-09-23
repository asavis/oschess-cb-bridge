# oschess-cb-bridge agent guidance

## Repository

- Canonical checkout: `/home/asavi/projects/oschess-cb-bridge`. Keep its `main`
  worktree clean; make every change in a dedicated worktree under
  `.worktrees/<name>` on a feature branch.
- This public repository is independent from the private `oschess` repository.
  Copy no code, prompts, data or configuration from `oschess` into it.
- Keep all GitHub-facing content in English: issues, pull requests, review
  comments, commit messages and release notes.
- Never commit a chess database, a PGN export of one, or anything derived from
  a user's database. `.gitignore` excludes the database extensions; tests use
  records built by hand.
- ChessBase is a trademark of ChessBase GmbH. This project is not affiliated
  with it. Name the format descriptively and never use the trademark as the
  name of a crate, binary or repository.

## Delivery workflow

The review gate follows the scheme of the `kidschessleague` project.

- Open a focused pull request for every change. Nothing is committed to `main`
  directly.
- Every change is reviewed by the backend — Codex or Claude — that did not
  write it. A review by the implementing backend is self-review whatever
  account it posts under, and never satisfies the gate. The review matrix is an
  owner decision:

  | Author | Reviewer |
  |---|---|
  | Claude | Codex `gpt-6-astra`, effort `xhigh` |
  | Codex | Claude `sonnet`, effort `high` |

- The implementing session is the delivery owner. It starts the review itself,
  never leaves that to the owner, and supplies only the fixed review contract in
  [docs/review.md](docs/review.md), links to the issue and pull request, and the
  exact head commit. It does not choose the review's crux or focus.
- The reviewer runs with its normal full tool permissions. The prompt, not a
  sandbox, restricts it to review work: it posts findings, `CLEAN` or `BLOCKED`
  in the pull request under its own GitHub identity and changes nothing else.
- Fix every confirmed finding, then ask the same reviewer backend to review the
  new head. The gate is satisfied only by a `CB-REVIEW: CLEAN <sha>` verdict
  for the current head commit, accompanied by a prose review naming the same
  commit. Any push after a clean verdict, a rebase included, needs a new review.
- When the reviewer's usual account is out of quota, rerun the same review at
  once on the owner's account (`CODEX_HOME=/home/asavi/.codex`) without changing
  the reviewing backend or its GitHub identity, and say which account ran it.
- Once the clean verdict is recorded, the implementing session squash-merges the
  pull request itself with `--match-head-commit <clean sha>`.

## Checks

This repository is public, so anyone can open a pull request, and nobody else
may trigger tests or builds (owner decision, 2026-09-23). The only workflow,
`.github/workflows/ci.yml`, runs after a merge: on a push to `main`, which only
collaborators can make, and only on our own runners (`bridge-local` for Linux,
`bridge-windows` for Windows). Never add a workflow, trigger or job that a pull
request, an issue, a comment or a fork can start, and never route a job to
GitHub's hosted runners. Before a merge, every check runs on our own hosts,
started by us: by the author before every push and by the reviewer on the
reviewed commit.

Before every push, in the pull request's worktree:

```
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
scripts/clippy-windows.sh
cargo test
```

`scripts/clippy-windows.sh` runs clippy for `x86_64-pc-windows-gnu`, which
checks the Windows-only code without linking it (`rustup target add
x86_64-pc-windows-gnu`). It stands in a stub for the Windows resource compiler
that the app's build script calls. Say in the pull request what could
not be run, such as tests of Windows-only behaviour.

A change to the reader is also run with `cbtool verify` over every local
database available, and the counts go in the pull request body. A reader change
that has not decoded real databases is not ready.

## Code

- Treat every database file as untrusted input: no panics, no unchecked
  indexing on file-derived values, bounded allocation.
- Findings about the format that differ from the published description go in
  [docs/format-notes.md](docs/format-notes.md) with the evidence counts.
