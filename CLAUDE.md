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
  pull request itself with `--match-head-commit <clean sha>`. This is the
  owner's standing authorization (2026-09-30): a pull request whose current
  head carries a `CB-REVIEW: CLEAN <sha>` verdict and the prose review naming
  that commit is reviewed, and an agent merges it into `main` without asking
  again.
- That merge is where an agent's authority ends. The version number, a version
  change in `crates/app/Cargo.toml`, a `v*` tag, publishing a draft release and
  a Microsoft Store submission each need the owner's explicit order naming the
  version. An agent never chooses a version, and a general request such as
  "ship it" or "send it to the Store" is not that order.

## Checks

This repository is public, so anyone can open a pull request, and nobody else
may trigger tests or builds (owner decision, 2026-09-23). The check workflow,
`.github/workflows/ci.yml`, runs after a merge: on a push to `main`, which only
collaborators can make, and only on our own runners (`bridge-local` for Linux,
`bridge-windows` for Windows). `.github/workflows/stress.yml` runs
`scripts/stress.py` at night on `bridge-local`, on a schedule, and only when
`main` has moved past the last commit a night covered (owner decision on #235,
2026-09-30); a collaborator can also start it by hand on `main`. Never add a
workflow, trigger or job
that a pull request, an issue, a comment or a fork can start, and never route a
job to GitHub's hosted runners. The one exception (owner decision on #23,
2026-09-24) is `.github/workflows/release.yml`: it runs only on a version tag
that a collaborator pushes, on GitHub's hosted Windows runners, because SignPath
requires every job before a signing request to run there
([docs/release.md](docs/release.md)). Before a merge, every check runs on our
own hosts, started by us: by the author before every push and by the reviewer
on the reviewed commit.

Before every push, in the pull request's worktree:

```
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
scripts/clippy-windows.sh
cargo test
```

`scripts/clippy-windows.sh` runs clippy for `x86_64-pc-windows-gnu`, which
checks the Windows-only code without linking it (rustup installs that target
with the toolchain, from `rust-toolchain.toml`). It stands in a stub for the
Windows resource compiler that the app's build script calls. Say in the pull
request what could not be run, such as tests of Windows-only behaviour.

A change to the reader is also run with `cbtool verify` over every local
database available, and the counts go in the pull request body. A reader change
that has not decoded real databases is not ready.

A test that fails now and then, or a change to connections, threads, time-outs
or waits, is run with `scripts/stress.py` on Linux, which loads the machine
itself (#217, #235). A fix of a load failure is accepted in three steps, which
its pull request reports:

1. The cause is named, with the code it comes from, and reproduced
   deterministically through a seam, a hook or an injected delay: the
   reproduction fails on the old code every time and passes on the new one, and
   stays in the suite when the seam is cheap.
2. Every test binary the change touches, under load:
   `python3 scripts/stress.py --bin <binary> --runs 200 --jobs 4
   --min-slowdown 5`, and all 200 runs pass. A campaign whose loaded runs took
   less than five times the run without the load is void (exit status 2) and
   runs again under a harder load (`--hogs`, `--hog-nice 0`).
3. The whole suite, as a smoke test:
   `python3 scripts/stress.py --runs 16 --jobs 8 --fail-fast`.

A failure in a test the change does not touch gets its own ticket and resets no
count. The ticket gives the test, the commit, the message and the kept log; the
stress command, the load average and the slowdown; whether the product, the
test's design or the harness failed, and whether a user could meet it; and the
cause with its reproduction, or the instrumentation that will name it.

## Code

- Treat every database file as untrusted input: no panics, no unchecked
  indexing on file-derived values, bounded allocation.
- Findings about the format that differ from the published description go in
  [docs/format-notes.md](docs/format-notes.md) with the evidence counts.

## API contract

`docs/api.md` is the contract with oschess, and every change to it costs a
release, a Store certification, and changes to the oschess client and its
fake bridge. Its "Compatibility" section holds the rules; in short:

- Send every record whole and as decoded: a game any answer names is a
  `/games` row with all its fields. Never trim a record for one screen.
- Only add fields. A removal or a new meaning is `/v2`.
- Acknowledge in the answer every parameter that narrows it, so that a
  client can tell an older bridge that ignored it.
- Make a count or a depth that a screen might want to change a bounded
  parameter, not a constant.

A pull request that changes `docs/api.md` names the rule each change follows.
