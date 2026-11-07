"""The worktree report and cleanup (#362): python3 -m unittest discover -s scripts"""

import importlib.util
import io
import json
import os
import shutil
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("worktrees", HERE / "worktrees.py")
worktrees = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(worktrees)

Pull = worktrees.Pull
GIT_ENV = {
    "GIT_AUTHOR_NAME": "t",
    "GIT_AUTHOR_EMAIL": "t@example.com",
    "GIT_COMMITTER_NAME": "t",
    "GIT_COMMITTER_EMAIL": "t@example.com",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
}


class FakeGitHub:
    def __init__(self, pulls=(), by_commit=None):
        self.listed = list(pulls)
        self.by_commit = by_commit or {}

    def pulls(self):
        return self.listed

    def commit_pulls(self, sha):
        return self.by_commit.get(sha, [])


class Repo(unittest.TestCase):
    """A repository with an origin, and worktrees in every state."""

    def setUp(self):
        patch = mock.patch.dict(os.environ, GIT_ENV)
        patch.start()
        self.addCleanup(patch.stop)
        self.tmp = Path(tempfile.mkdtemp()).resolve()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.repo = self.tmp / "repo"
        self.git("init", "-q", "--bare", str(self.tmp / "origin.git"), cwd=self.tmp)
        self.git("init", "-q", "-b", "main", str(self.repo), cwd=self.tmp)
        self.git("remote", "add", "origin", str(self.tmp / "origin.git"))
        self.commit("one")
        self.git("push", "-q", "origin", "main")
        self.git("fetch", "-q", "origin")

    def git(self, *args, cwd=None):
        return subprocess.run(["git", *args], cwd=cwd or self.repo, check=True, capture_output=True, text=True).stdout.strip()

    def commit(self, text, cwd=None):
        folder = cwd or self.repo
        (folder / "file").write_text(text)
        self.git("add", "file", cwd=folder)
        self.git("commit", "-q", "-m", text, cwd=folder)
        return self.git("rev-parse", "HEAD", cwd=folder)

    def add(self, name, *options, at="main"):
        self.git("worktree", "add", "-q", *options, str(self.repo / ".worktrees" / name), at)
        return self.repo / ".worktrees" / name

    def branch_with_commit(self, name, branch, text, push=True):
        tree = self.add(name, "-b", branch)
        sha = self.commit(text, cwd=tree)
        if push:
            self.git("push", "-q", "origin", branch, cwd=tree)
        return tree, sha

    def every_state(self):
        """Worktrees in each state and the GitHub that goes with them."""
        self.add("in-main", "--detach")
        _, merged = self.branch_with_commit("merged", "feat/a", "a")
        open_tree, earlier = self.branch_with_commit("open", "feat/b", "b1")
        head = self.commit("b2", cwd=open_tree)
        self.git("push", "-q", "origin", "feat/b", cwd=open_tree)
        self.add("earlier", "--detach", at=earlier)
        (self.add("dirty", "--detach") / "notes.txt").write_text("work")
        self.commit("local", cwd=self.add("local", "--detach"))
        self.add("locked", "--detach")
        self.git("worktree", "lock", str(self.repo / ".worktrees" / "locked"))
        busy = self.add("busy", "--detach")
        sleeper = subprocess.Popen(["sleep", "60"], cwd=busy)
        self.addCleanup(sleeper.wait)
        self.addCleanup(sleeper.kill)
        self.branch_with_commit("no-pr", "feat/c", "c", push=False)
        return FakeGitHub(
            pulls=[("feat/a", Pull(1, "MERGED", merged)), ("feat/b", Pull(2, "OPEN", head))],
            by_commit={earlier: [Pull(2, "OPEN", head)]},
        )

    def verdicts(self, github, min_idle=0, now=time.time):
        return {row.tree.name: (row.verdict, row.why) for row in worktrees.survey(self.repo, github, min_idle, now)}


class Verdicts(Repo):
    def test_each_state_gets_its_verdict(self):
        self.assertEqual(
            self.verdicts(self.every_state()),
            {
                "busy": ("keep", "in use"),
                "dirty": ("keep", "uncommitted changes"),
                "earlier": ("remove", "an earlier head of open #2"),
                "in-main": ("remove", "in origin/main"),
                "local": ("keep", "no ref and no pull request holds its commit"),
                "locked": ("keep", "locked"),
                "merged": ("remove", "#1 merged"),
                "no-pr": ("keep", "no pull request"),
                "open": ("keep", "open #2"),
            },
        )

    def test_a_worktree_that_moved_lately_is_kept(self):
        github = self.every_state()
        found = self.verdicts(github, min_idle=10 * worktrees.HOUR)
        self.assertEqual(found["merged"][0], "keep")
        self.assertTrue(found["merged"][1].startswith("moved 0.0 h ago"))

    def test_without_github_only_origin_main_says_done(self):
        self.every_state()
        found = self.verdicts(None)
        self.assertEqual(found["in-main"], ("remove", "in origin/main"))
        self.assertEqual(found["merged"], ("keep", "no pull request"))
        self.assertEqual(found["earlier"], ("keep", "no pull request"))

    def test_a_missing_folder_is_pruned(self):
        shutil.rmtree(self.add("gone", "--detach"))
        self.assertEqual(self.verdicts(None)["gone"], ("prune", "its folder is gone"))


class Clean(Repo):
    def names(self):
        listed = self.git("worktree", "list", "--porcelain").splitlines()
        return sorted(Path(line.split(" ", 1)[1]).name for line in listed if line.startswith("worktree ") and ".worktrees" in line)

    def test_a_dry_run_changes_nothing(self):
        github = self.every_state()
        before = self.names()
        told = io.StringIO()
        rows = worktrees.survey(self.repo, github, 0)
        self.assertEqual(worktrees.clean(self.repo, rows, False, None, out=told), 0)
        self.assertEqual(self.names(), before)
        self.assertIn("would remove merged", told.getvalue())

    def test_apply_removes_the_finished_ones_and_keeps_their_branches(self):
        github = self.every_state()
        rows = worktrees.survey(self.repo, github, 0)
        self.assertEqual(worktrees.clean(self.repo, rows, True, None, out=io.StringIO()), 0)
        self.assertEqual(self.names(), ["busy", "dirty", "local", "locked", "no-pr", "open"])
        self.assertIn("feat/a", self.git("branch", "--list", "feat/a"))

    def test_an_unused_target_is_emptied_but_the_stress_output_kept(self):
        tree, _ = self.branch_with_commit("open", "feat/b", "b")
        for path in ("target/debug/deps/libx.rlib", "target/stress/20261107T000000Z/run-1.log", "target/.rustc_info.json"):
            (tree / path).parent.mkdir(parents=True, exist_ok=True)
            (tree / path).write_text("x" * 5000)
        github = FakeGitHub(pulls=[("feat/b", Pull(2, "OPEN", "f" * 40))])
        later = lambda: time.time() + 48 * worktrees.HOUR  # noqa: E731
        rows = worktrees.survey(self.repo, github, 0, now=later)
        told = io.StringIO()
        worktrees.clean(self.repo, rows, True, 24 * worktrees.HOUR, out=told)
        self.assertEqual(sorted(p.name for p in (tree / "target").iterdir()), ["stress"])
        self.assertIn("emptied open/target", told.getvalue())

    def test_a_target_that_moved_lately_stays(self):
        tree, _ = self.branch_with_commit("open", "feat/b", "b")
        (tree / "target" / "debug").mkdir(parents=True)
        (tree / "target" / "debug" / "x").write_text("x" * 5000)
        rows = worktrees.survey(self.repo, FakeGitHub(), 0)
        worktrees.clean(self.repo, rows, True, 24 * worktrees.HOUR, out=io.StringIO())
        self.assertTrue((tree / "target" / "debug" / "x").exists())


class Remove(Repo):
    def test_removes_a_clean_worktree_whatever_its_pull_request_and_refuses_the_rest(self):
        github = self.every_state()
        told = io.StringIO()
        code = worktrees.remove(self.repo, ["open", "dirty", "local", "busy", "nothing"], github, out=told)
        self.assertEqual(code, 1)
        self.assertEqual(
            told.getvalue().splitlines(),
            [
                "removed open",
                "kept dirty: uncommitted changes",
                "kept local: no ref and no pull request holds its commit",
                "kept busy: in use",
                "kept nothing: no such worktree under .worktrees/",
            ],
        )
        self.assertFalse((self.repo / ".worktrees" / "open").exists())


class GitHubAnswers(unittest.TestCase):
    def github(self, answers):
        def run(command, **_):
            answer = answers[command[1]]
            return subprocess.CompletedProcess(command, 0 if answer is not None else 1, json.dumps(answer), "")

        return worktrees.GitHub("o/r", run=run)

    def test_reads_the_pull_requests_and_a_commits_pull_requests(self):
        github = self.github(
            {
                "pr": [{"number": 7, "state": "MERGED", "headRefName": "feat/x", "headRefOid": "a" * 40}],
                "api": [
                    {"number": 8, "state": "closed", "merged_at": "2026-11-07T01:00:00Z", "head": {"sha": "b" * 40}},
                    {"number": 9, "state": "closed", "merged_at": None, "head": {"sha": "c" * 40}},
                    {"number": 10, "state": "open", "merged_at": None, "head": {"sha": "d" * 40}},
                ],
            }
        )
        self.assertEqual(github.pulls(), [("feat/x", Pull(7, "MERGED", "a" * 40))])
        self.assertEqual(
            github.commit_pulls("e" * 40),
            [Pull(8, "MERGED", "b" * 40), Pull(9, "CLOSED", "c" * 40), Pull(10, "OPEN", "d" * 40)],
        )

    def test_a_failure_is_no_answer(self):
        github = self.github({"pr": None, "api": None})
        self.assertIsNone(github.pulls())
        self.assertEqual(github.commit_pulls("e" * 40), [])

    def test_names_the_repository_of_an_https_or_ssh_remote(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(os.environ, GIT_ENV):
            repo = Path(tmp)
            subprocess.run(["git", "init", "-q", tmp], check=True)
            for url in ("https://github.com/asavis/oschess-cb-bridge.git", "git@github.com:asavis/oschess-cb-bridge.git"):
                subprocess.run(["git", "remote", "remove", "origin"], cwd=repo, capture_output=True)
                subprocess.run(["git", "remote", "add", "origin", url], cwd=repo, check=True)
                self.assertEqual(worktrees.slug(repo), "asavis/oschess-cb-bridge")


if __name__ == "__main__":
    unittest.main()
