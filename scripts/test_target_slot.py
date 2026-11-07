"""The shared target slots (#362): python3 -m unittest discover -s scripts"""

import importlib.util
import io
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "target_slot.py"
SPEC = importlib.util.spec_from_file_location("target_slot", SCRIPT)
target_slot = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(target_slot)

HASH = "0123456789abcdef"


def write(path, text=""):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    return path


def workspace(folder, crates=("bridge", "app")):
    write(folder / "Cargo.toml", '[workspace]\nmembers = ["crates/*"]\nresolver = "3"\n')
    for name in crates:
        write(folder / "crates" / name / "Cargo.toml", f'[package]\nname = "{name}"\nversion = "0.1.0"\n')
    return folder


def git_init(folder):
    subprocess.run(["git", "init", "-q", str(folder)], check=True)
    return folder


def fingerprints(target):
    return sorted(str(p.relative_to(target)) for f in target_slot.fingerprint_folders(target) for p in f.iterdir())


class Temp(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.config = target_slot.Settings(root=self.tmp / "slots", slots=2, wait=0, max_bytes=10**12)


class Members(Temp):
    def test_reads_the_package_names_the_member_globs_find(self):
        ws = workspace(self.tmp / "ws", crates=("bridge", "cbtool"))
        write(ws / "crates" / "notes" / "README.md")
        self.assertEqual(target_slot.members(ws), ["bridge", "cbtool"])

    def test_a_workspace_without_a_readable_manifest_has_none(self):
        self.assertEqual(target_slot.workspace_members(self.tmp), [])


class DiskUsage(Temp):
    def test_counts_a_file_with_two_names_once(self):
        one = write(self.tmp / "t" / "deps" / "bridge-1", "x" * 100000)
        os.link(one, self.tmp / "t" / "bridge")
        self.assertEqual(target_slot.disk_usage(self.tmp / "t"), os.lstat(one).st_blocks * 512)


class Prepare(Temp):
    def slot(self, owner=None):
        folder = self.tmp / "slots" / "slot-1"
        folder.mkdir(parents=True)
        if owner:
            write(folder / "owner", f"{owner}\n")
        target = folder / "target"
        for unit in (
            f"debug/.fingerprint/bridge-{HASH}",
            f"debug/.fingerprint/serde-{HASH}",
            f"debug/.fingerprint/bridge-extra-{HASH}",
            f"release/.fingerprint/app-{HASH}",
            f"x86_64-pc-windows-gnu/debug/.fingerprint/app-{HASH}",
            f"x86_64-pc-windows-gnu/debug/.fingerprint/libc-{HASH}",
        ):
            write(target / unit / "lib-x", "1")
        write(target / "debug" / "deps" / f"libserde-{HASH}.rlib", "x" * 10000)
        return target_slot.Lease(folder, -1, slot=True)

    def test_another_worktrees_slot_loses_the_members_fingerprints_only(self):
        held = self.slot(owner="/w/other")
        target_slot.prepare(held, Path("/w/this"), ["bridge", "app"], 10**12)
        self.assertEqual(
            fingerprints(held.target),
            [
                f"debug/.fingerprint/bridge-extra-{HASH}",
                f"debug/.fingerprint/serde-{HASH}",
                f"x86_64-pc-windows-gnu/debug/.fingerprint/libc-{HASH}",
            ],
        )
        self.assertTrue((held.target / "debug" / "deps" / f"libserde-{HASH}.rlib").exists())
        self.assertEqual((held.folder / "owner").read_text(), "/w/this\n")

    def test_a_slot_with_no_owner_counts_as_another_worktrees(self):
        held = self.slot()
        target_slot.prepare(held, Path("/w/this"), ["bridge"], 10**12)
        self.assertNotIn(f"debug/.fingerprint/bridge-{HASH}", fingerprints(held.target))

    def test_the_same_worktrees_slot_is_left_as_it_is(self):
        held = self.slot(owner="/w/this")
        before = fingerprints(held.target)
        target_slot.prepare(held, Path("/w/this"), ["bridge", "app"], 10**12)
        self.assertEqual(fingerprints(held.target), before)

    def test_a_slot_past_the_limit_is_emptied(self):
        held = self.slot(owner="/w/this")
        told = io.StringIO()
        target_slot.prepare(held, Path("/w/this"), ["bridge"], 1000, out=told)
        self.assertFalse(held.target.exists())
        self.assertIn("more than the limit", told.getvalue())

    def test_unknown_members_or_layout_empty_the_slot(self):
        held = self.slot(owner="/w/other")
        target_slot.prepare(held, Path("/w/this"), [], 10**12)
        self.assertFalse(held.target.exists())
        write(held.target / "debug" / "deps" / "libx.rlib")
        write(held.folder / "owner", "/w/other\n")
        target_slot.prepare(held, Path("/w/this"), ["bridge"], 10**12)
        self.assertFalse(held.target.exists())


class Leases(Temp):
    def lease(self, ws, **kw):
        held = target_slot.lease(self.config, Path(ws), out=io.StringIO(), **kw)
        self.addCleanup(os.close, held.fd)
        return held

    def test_takes_the_slot_its_worktree_had_last(self):
        write(self.config.root / "slot-2" / "owner", "/w/a\n")
        self.assertEqual(self.lease("/w/a").folder.name, "slot-2")
        self.assertEqual(self.lease("/w/b").folder.name, "slot-1")

    def test_skips_a_slot_another_command_holds(self):
        first = self.lease("/w/a")
        second = self.lease("/w/a")
        self.assertEqual({first.folder.name, second.folder.name}, {"slot-1", "slot-2"})

    def test_waits_for_a_slot_then_builds_in_a_private_folder(self):
        self.lease("/w/a"), self.lease("/w/b")
        naps = []
        clock = iter(range(0, 1000, 3))
        config = target_slot.Settings(self.config.root, 2, wait=10, max_bytes=10**12)
        held = target_slot.lease(config, Path("/w/c"), out=io.StringIO(), sleep=naps.append, clock=lambda: next(clock))
        self.addCleanup(os.close, held.fd)
        self.assertFalse(held.slot)
        self.assertTrue(held.folder.name.startswith("private-"))
        self.assertTrue(naps)

    def test_a_private_folder_nobody_holds_is_swept(self):
        root = self.config.root
        stale = write(root / "private-old" / "target" / "x").parent.parent
        held = write(root / "private-held" / "target" / "x").parent.parent
        young = write(root / "private-young" / "target" / "x").parent.parent
        fd = target_slot.take(held / "lock")
        self.addCleanup(os.close, fd)
        old = time.time() - 3600
        for folder in (stale, held):
            os.utime(folder, (old, old))
        target_slot.sweep(root)
        self.assertEqual(sorted(p.name for p in root.iterdir()), ["private-held", "private-young"])


class Main(Temp):
    """The script as a command, in a git worktree of its own."""

    def setUp(self):
        super().setUp()
        self.ws = git_init(workspace(self.tmp / "ws"))
        self.env = dict(os.environ, CB_TARGET_ROOT=str(self.config.root), CB_TARGET_SLOTS="1", CB_TARGET_WAIT="0")
        for name in ("CARGO_TARGET_DIR", "CB_TARGET_SLOT", "CB_TARGET_SLOT_WORKTREE"):
            self.env.pop(name, None)

    def script(self, *args, cwd=None, env=None):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *args],
            cwd=cwd or self.ws,
            env=env or self.env,
            capture_output=True,
            text=True,
        )

    def test_runs_the_command_in_the_slot_and_passes_its_status_on(self):
        shown = self.script(sys.executable, "-c", "import os, sys; print(os.environ['CARGO_TARGET_DIR']); sys.exit(3)")
        self.assertEqual(shown.returncode, 3)
        self.assertEqual(shown.stdout.strip(), str(self.config.root / "slot-1" / "target"))
        self.assertEqual((self.config.root / "slot-1" / "owner").read_text(), f"{self.ws.resolve()}\n")

    def test_a_command_inside_a_lease_for_the_same_worktree_keeps_it(self):
        inner = f"import subprocess, sys; sys.exit(subprocess.run([sys.executable, {str(SCRIPT)!r}, 'printenv', 'CB_TARGET_SLOT']).returncode)"
        shown = self.script(sys.executable, "-c", inner)
        self.assertEqual(shown.returncode, 0, shown.stderr)
        self.assertEqual(shown.stdout.strip(), "slot-1")

    def test_a_lease_for_another_worktree_is_not_reused(self):
        env = dict(self.env, CARGO_TARGET_DIR="/elsewhere", CB_TARGET_SLOT="slot-9", CB_TARGET_SLOT_WORKTREE="/w/other")
        shown = self.script("printenv", "CB_TARGET_SLOT", env=env)
        self.assertEqual(shown.stdout.strip(), "slot-1")

    def inherited(self, token):
        """The environment a process of an earlier lease of slot-1 had."""
        slot = self.config.root / "slot-1"
        slot.mkdir(parents=True, exist_ok=True)
        write(slot / "lease", f"{token}\n")
        return dict(
            self.env,
            CARGO_TARGET_DIR=str(slot / "target"),
            CB_TARGET_SLOT="slot-1",
            CB_TARGET_SLOT_WORKTREE=str(self.ws.resolve()),
            CB_TARGET_SLOT_TOKEN=token,
        )

    def test_a_lease_that_has_ended_is_not_reused(self):
        env = self.inherited("ended")
        shown = self.script("printenv", "CB_TARGET_SLOT_TOKEN", env=env)
        self.assertEqual(shown.returncode, 0, shown.stderr)
        self.assertNotIn(shown.stdout.strip(), ("", "ended"))
        self.assertEqual((self.config.root / "slot-1" / "lease").read_text().strip(), shown.stdout.strip())

    def test_a_slot_passed_on_to_another_lease_is_not_reused(self):
        env = self.inherited("old")
        holder = target_slot.take(self.config.root / "slot-1" / "lock")
        self.addCleanup(os.close, holder)
        write(self.config.root / "slot-1" / "lease", "new\n")
        shown = self.script("printenv", "CARGO_TARGET_DIR", env=env)
        self.assertIn("private-", shown.stdout)

    def test_a_held_lease_with_its_token_is_reused(self):
        env = self.inherited("live")
        holder = target_slot.take(self.config.root / "slot-1" / "lock")
        self.addCleanup(os.close, holder)
        shown = self.script("printenv", "CB_TARGET_SLOT_TOKEN", env=env)
        self.assertEqual(shown.stdout.strip(), "live")

    def test_a_private_folder_stays_while_a_process_of_the_command_holds_it(self):
        slot = self.config.root / "slot-1"
        slot.mkdir(parents=True)
        holder = target_slot.take(slot / "lock")
        self.addCleanup(os.close, holder)
        shown = self.script("sh", "-c", 'sleep 2 >/dev/null 2>&1 & printf %s "$CARGO_TARGET_DIR"')
        private = Path(shown.stdout).parent
        self.assertTrue(private.name.startswith("private-"))
        self.assertTrue(private.exists())
        deadline = time.monotonic() + 10
        while (fd := target_slot.take(private / "lock")) is None and time.monotonic() < deadline:
            time.sleep(0.1)
        self.assertIsNotNone(fd)
        os.close(fd)
        target_slot.sweep(self.config.root, now=lambda: time.time() + 2 * target_slot.PRIVATE_GRACE)
        self.assertFalse(private.exists())

    def test_dash_c_names_the_worktree(self):
        shown = self.script("-C", str(self.ws), "printenv", "CB_TARGET_SLOT_WORKTREE", cwd=self.tmp)
        self.assertEqual(shown.stdout.strip(), str(self.ws.resolve()))

    def test_a_private_folder_goes_when_the_command_ends(self):
        slot = self.config.root / "slot-1"
        slot.mkdir(parents=True)
        holder = target_slot.take(slot / "lock")
        self.addCleanup(os.close, holder)
        shown = self.script("printenv", "CARGO_TARGET_DIR")
        self.assertIn("private-", shown.stdout)
        self.assertFalse(Path(shown.stdout.strip()).parent.exists())

    def test_without_a_usable_root_the_command_builds_in_its_worktree(self):
        env = dict(self.env, CB_TARGET_ROOT=str(write(self.tmp / "a-file") / "slots"))
        shown = self.script("sh", "-c", 'echo "${CARGO_TARGET_DIR:-unset}"', env=env)
        self.assertEqual(shown.returncode, 0)
        self.assertEqual(shown.stdout.strip(), "unset")


@unittest.skipUnless(shutil.which("cargo"), "needs cargo")
class RealCargo(unittest.TestCase):
    """A slot that passes to another worktree builds that worktree's code, where
    a shared target folder would hand it the other worktree's build."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)

    def worktree(self, name, says, age=0):
        ws = workspace(self.tmp / name, crates=("bridge",))
        shutil.copy(HERE.parent / "rust-toolchain.toml", ws)
        main = write(ws / "crates" / "bridge" / "src" / "main.rs", f'fn main() {{ println!("{says}"); }}\n')
        git_init(ws)
        moment = time.time() - age
        for path in (main, ws / "Cargo.toml", ws / "crates" / "bridge" / "Cargo.toml"):
            os.utime(path, (moment, moment))
        return ws

    def test_the_older_checkout_runs_its_own_code(self):
        older = self.worktree("older", "older", age=3600)
        newer = self.worktree("newer", "newer")
        env = dict(os.environ, CB_TARGET_ROOT=str(self.tmp / "slots"), CB_TARGET_SLOTS="1", CB_TARGET_WAIT="0")
        for name in ("CARGO_TARGET_DIR", "CB_TARGET_SLOT", "CB_TARGET_SLOT_WORKTREE"):
            env.pop(name, None)
        said = {}
        for ws in (newer, older):
            run = subprocess.run(
                [sys.executable, str(SCRIPT), "cargo", "run", "-q", "--offline"], cwd=ws, env=env, capture_output=True, text=True
            )
            self.assertEqual(run.returncode, 0, run.stderr)
            said[ws.name] = run.stdout.strip()
        self.assertEqual(said, {"newer": "newer", "older": "older"})


if __name__ == "__main__":
    unittest.main()
