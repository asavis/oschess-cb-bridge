#!/usr/bin/env python3
"""Lists the worktrees under .worktrees/ and removes the finished ones (#362).
Linux only.

    python3 scripts/worktrees.py                   each worktree: size, idle time, verdict
    python3 scripts/worktrees.py clean             what `clean --apply` would do
    python3 scripts/worktrees.py clean --apply     do it
    python3 scripts/worktrees.py remove <name>...  remove these, finished or not

`clean` removes a worktree only when all of these hold:

- nothing in it is uncommitted, untracked files included; ignored files such as
  target/ are no work;
- no process runs in it or has a file of it open;
- `git worktree lock` does not hold it;
- nothing in it has moved for an hour (`--idle`);
- a ref or a pull request holds its commit, so that no commit is lost;
- and its work is done: its commit is in origin/main, or every pull request it
  belongs to is merged or closed, or it is a detached checkout of an earlier
  head of an open pull request, which the pull request has moved past.

The pull requests a worktree belongs to are those whose branch it has checked
out, whose head it is, or that GitHub lists for its commit. When `gh` fails,
only origin/main says that a worktree is done.

`clean` also empties the target/ folder of a worktree that nothing uses and
nothing has moved in for a day (`--target-idle`), all but target/stress, where
the stress runner keeps the output of failed runs. Builds go to the shared
slots of scripts/target_slot.py; a target/ of a worktree's own is left from
before them, or from a build outside a slot.

`remove` takes worktrees by their folder's name and removes each one that is
clean, unused and unlocked and whose commit a ref or a pull request holds,
whether its work is done or not. Neither command deletes a branch, and
`clean --apply` ends with `git worktree prune`.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

HOUR = 3600
# The output the stress runner keeps in a worktree's target/.
KEPT_IN_TARGET = {"stress"}


def git(*args, cwd=None):
    """Runs git without taking the index lock, so that a look changes nothing."""
    return subprocess.run(
        ["git", *args], cwd=cwd, capture_output=True, text=True, env=dict(os.environ, GIT_OPTIONAL_LOCKS="0")
    )


@dataclass
class Tree:
    path: Path
    head: str
    branch: str | None
    locked: bool
    prunable: bool

    @property
    def name(self):
        return self.path.name


@dataclass(frozen=True)
class Pull:
    number: int
    state: str  # OPEN, CLOSED or MERGED
    head: str


def repository(cwd=None):
    """The main checkout of the repository the current folder belongs to."""
    found = git("rev-parse", "--path-format=absolute", "--git-common-dir", cwd=cwd)
    if found.returncode != 0:
        sys.exit("worktrees: not inside a git repository")
    return Path(found.stdout.strip()).parent


def trees(repo):
    """The worktrees directly under the repository's .worktrees/."""
    root = repo / ".worktrees"
    found = []
    for block in git("worktree", "list", "--porcelain", cwd=repo).stdout.strip().split("\n\n"):
        fields = dict(line.partition(" ")[::2] for line in block.splitlines())
        if "worktree" not in fields or Path(fields["worktree"]).parent != root:
            continue
        branch = fields.get("branch")
        found.append(
            Tree(
                Path(fields["worktree"]),
                fields.get("HEAD", ""),
                branch.removeprefix("refs/heads/") if branch else None,
                "locked" in fields,
                "prunable" in fields,
            )
        )
    return sorted(found, key=lambda tree: tree.name)


def dirty(tree):
    """Whether the worktree has uncommitted changes, untracked files included.
    A worktree git cannot read counts as dirty."""
    shown = git("status", "--porcelain", "--untracked-files=normal", cwd=tree.path)
    return shown.returncode != 0 or bool(shown.stdout.strip())


def open_paths(own=None):
    """Every process's working folder, executable and open files, but this
    process's own."""
    own = own or os.getpid()
    paths = []
    for entry in os.scandir("/proc"):
        if not entry.name.isdigit() or int(entry.name) == own:
            continue
        links = [f"{entry.path}/cwd", f"{entry.path}/exe"]
        try:
            links += [f"{entry.path}/fd/{fd}" for fd in os.listdir(f"{entry.path}/fd")]
        except OSError:
            pass
        for link in links:
            try:
                paths.append(os.readlink(link))
            except OSError:
                pass
    return paths


def in_use(tree, paths):
    inside = str(tree.path)
    return any(path == inside or path.startswith(inside + "/") for path in paths)


def last_change(tree):
    """When anything in the worktree last moved: its folder, its git state
    (HEAD, index, reflog) and its target/ folders."""
    gitdir = git("rev-parse", "--absolute-git-dir", cwd=tree.path).stdout.strip()
    target = tree.path / "target"
    candidates = [tree.path, target, *target.glob("*"), *target.glob("*/deps"), *target.glob("*/*/deps")]
    if gitdir:
        candidates += [Path(gitdir) / "HEAD", Path(gitdir) / "index", Path(gitdir) / "logs" / "HEAD"]
    times = []
    for path in candidates:
        try:
            times.append(path.stat().st_mtime)
        except OSError:
            pass
    return max(times, default=0.0)


def held(repo, sha, pulls):
    """Whether removing a checkout of `sha` keeps the commit: a ref holds it,
    or a pull request does."""
    if pulls:
        return True
    refs = git("for-each-ref", "--count=1", "--contains", sha, "--format=%(refname)", cwd=repo)
    return refs.returncode == 0 and bool(refs.stdout.strip())


def in_main(repo, sha):
    return git("merge-base", "--is-ancestor", sha, "origin/main", cwd=repo).returncode == 0


def disk_usage(path):
    """The bytes a folder takes on the disk, 0 when it does not exist."""
    if not path.exists():
        return 0
    shown = subprocess.run(["du", "-s", "-B1", str(path)], capture_output=True, text=True)
    try:
        return int(shown.stdout.split()[0])
    except (IndexError, ValueError):
        return 0


class GitHub:
    """The repository's pull requests, through `gh`. Each answer is None when
    `gh` fails."""

    def __init__(self, slug, run=subprocess.run):
        self.slug = slug
        self.run = run
        self.by_commit = {}

    def gh(self, *args):
        shown = self.run(["gh", *args], capture_output=True, text=True)
        if shown.returncode != 0:
            return None
        try:
            return json.loads(shown.stdout)
        except json.JSONDecodeError:
            return None

    def pulls(self):
        found = self.gh(
            "pr", "list", "--repo", self.slug, "--state", "all", "--limit", "1000",
            "--json", "number,state,headRefName,headRefOid",
        )  # fmt: skip
        if found is None:
            return None
        return [(p["headRefName"], Pull(p["number"], p["state"], p["headRefOid"])) for p in found]

    def commit_pulls(self, sha):
        if sha not in self.by_commit:
            found = self.gh("api", f"repos/{self.slug}/commits/{sha}/pulls")
            self.by_commit[sha] = [
                Pull(p["number"], "MERGED" if p.get("merged_at") else p["state"].upper(), p["head"]["sha"])
                for p in found or []
            ]
        return self.by_commit[sha]


def slug(repo):
    """owner/name of the GitHub repository `origin` names, or None."""
    url = git("remote", "get-url", "origin", cwd=repo).stdout.strip()
    found = re.search(r"github\.com[:/](.+?)(?:\.git)?/?$", url)
    return found.group(1) if found else None


def belongs(tree, listed, github):
    """The pull requests the worktree belongs to."""
    found = {}
    for branch, pull in listed or []:
        if pull.head == tree.head or (tree.branch and branch == tree.branch):
            found[pull.number] = pull
    if github:
        for pull in github.commit_pulls(tree.head):
            found.setdefault(pull.number, pull)
    return sorted(found.values(), key=lambda pull: pull.number)


def numbers(pulls):
    return ", ".join(f"#{pull.number}" for pull in pulls)


def judge(tree, *, dirty, busy, idle, kept, merged, pulls, min_idle):
    """("remove" | "keep" | "prune", why)."""
    if tree.prunable:
        return "prune", "its folder is gone"
    if tree.locked:
        return "keep", "locked"
    if dirty:
        return "keep", "uncommitted changes"
    if busy:
        return "keep", "in use"
    if not kept:
        return "keep", "no ref and no pull request holds its commit"
    if idle < min_idle:
        return "keep", f"moved {idle / HOUR:.1f} h ago"
    if merged:
        return "remove", "in origin/main"
    if pulls:
        live = [pull for pull in pulls if pull.state == "OPEN"]
        if not live:
            return "remove", f"{numbers(pulls)} " + "/".join(sorted({p.state.lower() for p in pulls}))
        if tree.branch is None and all(pull.head != tree.head for pull in live):
            return "remove", f"an earlier head of open {numbers(live)}"
        return "keep", f"open {numbers(live)}"
    return "keep", "no pull request"


@dataclass
class Row:
    tree: Tree
    verdict: str
    why: str
    size: int
    target: int
    idle: float
    busy: bool


def survey(repo, github, min_idle, now=time.time):
    """Each worktree with its verdict, sizes and idle time."""
    listed = github.pulls() if github else None
    if github and listed is None:
        print("worktrees: gh failed; only origin/main says a worktree is done", file=sys.stderr)
        github = None
    paths = open_paths()
    rows = []
    for tree in trees(repo):
        if tree.prunable:
            rows.append(Row(tree, "prune", "its folder is gone", 0, 0, 0.0, False))
            continue
        pulls = belongs(tree, listed, github)
        busy = in_use(tree, paths)
        idle = now() - last_change(tree)
        verdict, why = judge(
            tree,
            dirty=dirty(tree),
            busy=busy,
            idle=idle,
            kept=held(repo, tree.head, pulls),
            merged=in_main(repo, tree.head),
            pulls=pulls,
            min_idle=min_idle,
        )
        rows.append(Row(tree, verdict, why, disk_usage(tree.path), disk_usage(tree.path / "target"), idle, busy))
    return rows


def gigabytes(size):
    return f"{size / 1e9:.1f} GB" if size >= 1e8 else f"{size / 1e6:.0f} MB"


def report(rows, out=sys.stdout):
    print(f"{'NAME':<24} {'SIZE':>9} {'TARGET':>9} {'IDLE':>8}  VERDICT", file=out)
    for row in rows:
        idle = f"{row.idle / HOUR:.1f} h"
        line = f"{row.tree.name:<24} {gigabytes(row.size):>9} {gigabytes(row.target):>9} {idle:>8}  {row.verdict}: {row.why}"
        print(line, file=out)
    total = sum(row.size for row in rows)
    going = sum(row.size for row in rows if row.verdict == "remove")
    print(f"{len(rows)} worktrees, {gigabytes(total)}; finished: {gigabytes(going)}", file=out)


def empty_target(tree):
    """Removes everything in the worktree's target/ but what KEPT_IN_TARGET
    names, and returns the bytes it took."""
    target = tree.path / "target"
    freed = 0
    for entry in target.iterdir() if target.is_dir() else []:
        if entry.name in KEPT_IN_TARGET:
            continue
        freed += disk_usage(entry)
        if entry.is_dir() and not entry.is_symlink():
            shutil.rmtree(entry, ignore_errors=True)
        else:
            entry.unlink(missing_ok=True)
    return freed


def stale_target(row, target_idle):
    return (
        row.verdict == "keep"
        and not row.busy
        and not row.tree.locked
        and row.idle >= target_idle
        and row.target > 0
        and any(entry.name not in KEPT_IN_TARGET for entry in (row.tree.path / "target").iterdir())
    )


def clean(repo, rows, apply, target_idle, out=sys.stdout):
    """Removes the finished worktrees and empties the stale targets, or says
    what it would do. Returns 1 when a removal failed."""
    word = "removed" if apply else "would remove"
    failed = freed = 0
    for row in rows:
        if row.verdict != "remove":
            continue
        print(f"{word} {row.tree.name} ({gigabytes(row.size)}): {row.why}", file=out)
        if apply:
            gone = git("worktree", "remove", str(row.tree.path), cwd=repo)
            if gone.returncode != 0:
                print(f"  failed: {gone.stderr.strip()}", file=out)
                failed += 1
                continue
        freed += row.size
    if target_idle is not None:
        for row in rows:
            if stale_target(row, target_idle):
                size = empty_target(row.tree) if apply else row.target
                print(f"{'emptied' if apply else 'would empty'} {row.tree.name}/target ({gigabytes(size)})", file=out)
                freed += size
    if apply:
        git("worktree", "prune", cwd=repo)
    print(f"{'freed' if apply else 'would free'} {gigabytes(freed)}", file=out)
    return 1 if failed else 0


def remove(repo, names, github, out=sys.stdout):
    """Removes the named worktrees that are clean, unused and unlocked and
    whose commit is held."""
    known = {tree.name: tree for tree in trees(repo)}
    listed = github.pulls() if github else None
    paths = open_paths()
    failed = 0
    for name in names:
        tree = known.get(name)
        why = None
        if tree is None:
            why = "no such worktree under .worktrees/"
        elif tree.prunable:
            print(f"pruned {name}: its folder was gone", file=out)
            continue
        elif tree.locked:
            why = "locked"
        elif dirty(tree):
            why = "uncommitted changes"
        elif in_use(tree, paths):
            why = "in use"
        elif not held(repo, tree.head, belongs(tree, listed, github)):
            why = "no ref and no pull request holds its commit"
        if why is None:
            gone = git("worktree", "remove", str(tree.path), cwd=repo)
            why = None if gone.returncode == 0 else gone.stderr.strip()
        if why:
            print(f"kept {name}: {why}", file=out)
            failed += 1
        else:
            print(f"removed {name}", file=out)
    git("worktree", "prune", cwd=repo)
    return 1 if failed else 0


def main(argv):
    parser = argparse.ArgumentParser(description="The worktrees under .worktrees/ and their cleanup.")
    parser.add_argument("--no-fetch", action="store_true", help="judge by the origin/main already fetched")
    parser.add_argument("--no-github", action="store_true", help="ask GitHub nothing; only origin/main says done")
    commands = parser.add_subparsers(dest="command")
    cleaning = commands.add_parser("clean", help="remove the finished worktrees")
    cleaning.add_argument("--apply", action="store_true", help="remove them; without it, only say what would go")
    cleaning.add_argument("--idle", type=float, default=1, help="hours a finished worktree has not moved (1)")
    cleaning.add_argument("--target-idle", type=float, default=24, help="hours before an unused target/ is emptied (24)")
    cleaning.add_argument("--keep-targets", action="store_true", help="empty no target/")
    removing = commands.add_parser("remove", help="remove these worktrees, finished or not")
    removing.add_argument("names", nargs="+")
    args = parser.parse_args(argv)

    repo = repository()
    if not args.no_fetch and git("fetch", "--quiet", "origin", cwd=repo).returncode != 0:
        print("worktrees: git fetch failed; judging by the origin/main already fetched", file=sys.stderr)
    name = None if args.no_github else slug(repo)
    github = GitHub(name) if name and shutil.which("gh") else None
    if args.command == "remove":
        return remove(repo, args.names, github)
    min_idle = args.idle * HOUR if args.command == "clean" else HOUR
    rows = survey(repo, github, min_idle)
    if args.command == "clean":
        return clean(repo, rows, args.apply, None if args.keep_targets else args.target_idle * HOUR)
    report(rows)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
