#!/usr/bin/env python3
"""Runs a command with its Cargo builds in one of a few shared target folders,
which the command holds alone while it runs (#362). Linux only.

    python3 scripts/target_slot.py cargo test
    python3 scripts/target_slot.py python3 scripts/stress.py --runs 16 --jobs 8 --fail-fast
    python3 scripts/target_slot.py -C <worktree> codex exec -C <worktree> ...
    python3 scripts/target_slot.py --status

The slot is for the worktree the command runs in, or the one `-C` names.

A worktree's own target/ takes about 8 GB, and on 2026-11-07 thirty of them
under .worktrees/ filled the disk. One target folder for every worktree is no
answer, as Cargo 1.98 behaves:

- it names a workspace member's artifacts after the member's path inside the
  workspace, which is the same in every worktree, so every worktree's
  `bridge` writes the same files;
- it takes a member's build as fresh when no source file is newer than the
  build, so a worktree checked out before another worktree built takes that
  build as its own, and its tests run the other commit's code;
- it holds the folder's lock while it builds, not while `cargo test` runs the
  binaries it built, which another worktree's build may then replace.

So the folders are slots, ~/.cache/oschess-cb-bridge/targets/slot-<n>, and a
command holds one alone from its start to its end: its builds, the tests they
run and the binaries a test starts. The dependencies' builds stay in a slot from
one command to the next, and they are most of a build. When a slot passes to
another worktree, the workspace members' fingerprints go first, so that Cargo
builds the members again from this worktree's sources. A command takes the slot
its worktree had last when that one is free. When every slot is taken it waits,
then builds in a folder of its own, which goes once no process of the command
holds its lock any more.

A command started through this script inside another one that holds a slot
for the same worktree, such as a check inside a review session started through
this script, joins that lease: it holds the slot's lock, shared, with a
descriptor of its own, for as long as it lives, so the slot stays this
worktree's even if the command that took it ends first. It joins only while
the slot still carries the lease's token; a process that outlived its lease
finds the slot passed on under another token, and takes a lease of its own.

    CB_TARGET_ROOT          the slots' folder (~/.cache/oschess-cb-bridge/targets)
    CB_TARGET_SLOTS         how many slots there are (4)
    CB_TARGET_WAIT          how long to wait for a free slot, in seconds (120)
    CB_TARGET_SLOT_MAX_GB   a slot larger than this is emptied before a command (16)

A slot of this workspace holds about 10 GB after the pre-push checks: the
debug builds and tests, clippy's and the Windows clippy check's. So the slots
take some 40 GB, 64 GB at most, however many worktrees there are, and a private
folder takes its 10 GB only while its command runs.
"""

import fcntl
import os
import re
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
from dataclasses import dataclass
from glob import glob
from pathlib import Path

NAME = "target_slot"
# A private folder younger than this may still be waiting for its lock.
PRIVATE_GRACE = 60


@dataclass(frozen=True)
class Settings:
    root: Path
    slots: int
    wait: float
    max_bytes: int


def settings(env=os.environ):
    """The slots' folder, their number, the wait and the size limit, from the
    environment."""
    cache = Path(env.get("XDG_CACHE_HOME") or Path.home() / ".cache")
    return Settings(
        root=Path(env.get("CB_TARGET_ROOT") or cache / "oschess-cb-bridge" / "targets"),
        slots=max(1, int(env.get("CB_TARGET_SLOTS") or 4)),
        wait=max(0.0, float(env.get("CB_TARGET_WAIT") or 120)),
        max_bytes=int(float(env.get("CB_TARGET_SLOT_MAX_GB") or 16) * 1e9),
    )


@dataclass
class Lease:
    """A held folder: a slot, or a private folder that goes at the end. `fd`
    holds the folder's lock, and `token` marks the folder as this lease's."""

    folder: Path
    fd: int
    slot: bool
    token: str = ""

    @property
    def target(self):
        return self.folder / "target"


def worktree(cwd=None):
    """The top folder of the worktree the command runs in."""
    found = subprocess.run(["git", "rev-parse", "--show-toplevel"], cwd=cwd, capture_output=True, text=True)
    if found.returncode != 0:
        sys.exit(f"{NAME}: not inside a git worktree")
    return Path(found.stdout.strip()).resolve()


def members(workspace):
    """The package names of the workspace's members, as Cargo.toml's
    `members` globs name them, and of the root package if there is one."""
    manifest = tomllib.loads((workspace / "Cargo.toml").read_text())
    names = []
    for pattern in manifest.get("workspace", {}).get("members", []):
        for folder in sorted(glob(str(workspace / pattern))):
            member = Path(folder) / "Cargo.toml"
            if member.is_file():
                name = tomllib.loads(member.read_text()).get("package", {}).get("name")
                if name:
                    names.append(name)
    root = manifest.get("package", {}).get("name")
    if root:
        names.append(root)
    return names


def fingerprint_folders(target):
    """The `.fingerprint` folders of every profile, for the host and for each
    `--target`."""
    found = [*target.glob("*/.fingerprint"), *target.glob("*/*/.fingerprint")]
    return sorted(folder for folder in found if folder.is_dir())


def drop_members(target, names):
    """Removes the named packages' fingerprints, so that Cargo builds those
    packages again, and returns how many went. A unit's fingerprint folder is
    its package's name and a 16-digit hash."""
    unit = re.compile(r"^(?:%s)-[0-9a-f]{16}$" % "|".join(re.escape(name) for name in names))
    dropped = 0
    for folder in fingerprint_folders(target):
        for entry in folder.iterdir():
            if unit.match(entry.name):
                shutil.rmtree(entry)
                dropped += 1
    return dropped


def disk_usage(folder):
    """The bytes the files under `folder` take on the disk, a file with several
    names (Cargo links each binary into its profile's folder) counted once."""
    total = 0
    seen = set()
    for parent, _, files in os.walk(folder):
        for name in files:
            try:
                info = os.lstat(os.path.join(parent, name))
            except OSError:
                continue
            if info.st_nlink > 1:
                if (info.st_dev, info.st_ino) in seen:
                    continue
                seen.add((info.st_dev, info.st_ino))
            total += info.st_blocks * 512
    return total


def gigabytes(size):
    return f"{size / 1e9:.1f} GB"


def owner(folder):
    """The worktree that used the slot last, or None."""
    try:
        return (folder / "owner").read_text().strip() or None
    except OSError:
        return None


def prepare(lease, workspace, names, max_bytes, out=sys.stderr):
    """Makes a slot right for `workspace`: empty when it has grown past
    `max_bytes`, without the members' fingerprints when another worktree used it
    last, and without anything when it holds no fingerprints Cargo would read
    or the members are unknown."""
    target = lease.target
    if target.exists():
        size = disk_usage(target)
        if size > max_bytes:
            print(f"{NAME}: {lease.folder.name} holds {gigabytes(size)}, more than the limit: emptying it", file=out)
            shutil.rmtree(target)
        elif owner(lease.folder) != str(workspace):
            if names and fingerprint_folders(target):
                drop_members(target, names)
            else:
                shutil.rmtree(target)
    (lease.folder / "owner").write_text(f"{workspace}\n")


def take(lock):
    """Takes the lock in the file `lock` alone, without waiting: its
    descriptor, or None when any process holds it, alone or shared."""
    fd = os.open(lock, os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(fd)
        return None
    return fd


def sweep(root, now=time.time):
    """Removes the private folders whose command has ended without removing
    them, as one that was killed."""
    for folder in root.glob("private-*"):
        try:
            if now() - folder.stat().st_mtime < PRIVATE_GRACE:
                continue
            fd = take(folder / "lock")
        except OSError:
            continue
        if fd is not None:
            shutil.rmtree(folder, ignore_errors=True)
            os.close(fd)


def acquire(folder, slot):
    """Takes `folder` for a new lease, or returns None when a process holds it.

    The holders of a lease hold the folder's lock shared, so that a command
    inside the lease can join it with a descriptor of its own. A new lease takes
    the lock alone, which succeeds only when no process holds it at all, marks
    the folder with its token, then shares the lock. The gate keeps two new
    leases from both finding the lock free while one of them turns it from
    exclusive to shared."""
    gate = os.open(folder / "gate", os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o644)
    try:
        fcntl.flock(gate, fcntl.LOCK_EX)
        fd = take(folder / "lock")
        if fd is None:
            return None
        token = secrets.token_hex(8)
        (folder / "lease").write_text(f"{token}\n")
        fcntl.flock(fd, fcntl.LOCK_SH)
        return Lease(folder, fd, slot, token)
    finally:
        os.close(gate)


def join(env, workspace, root, tries=40, nap=0.05):
    """Joins the lease the environment names for `workspace`: a descriptor
    holding the folder's lock shared, for this process and the command it
    becomes, or None when there is no such lease to join. The folder must still
    carry the lease's token once the lock is held: a process that outlived its
    lease finds the folder passed on under another token. A lease that ended
    and that no other lease has taken since is this worktree's still, and
    joining it holds it again."""
    name, token = env.get("CB_TARGET_SLOT"), env.get("CB_TARGET_SLOT_TOKEN")
    if env.get("CB_TARGET_SLOT_WORKTREE") != str(workspace) or not name or not token or "/" in name:
        return None
    folder = root / name
    if env.get("CARGO_TARGET_DIR") != str(folder / "target"):
        return None
    try:
        fd = os.open(folder / "lock", os.O_RDWR | os.O_CLOEXEC)
    except OSError:
        return None
    # A new lease or a look (--status, a sweep) holds the lock alone for a
    # moment; wait that out.
    for _ in range(tries):
        try:
            fcntl.flock(fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
            break
        except BlockingIOError:
            time.sleep(nap)
    else:
        os.close(fd)
        return None
    try:
        ours = (folder / "lease").read_text().strip() == token
    except OSError:
        ours = False
    if not ours:
        os.close(fd)
        return None
    return fd


def remove_private(folder, out=sys.stderr):
    """Removes a private folder, unless a process its command started still
    holds its lock: sweep() removes it once none does."""
    try:
        fd = take(folder / "lock")
    except OSError:
        return
    if fd is None:
        print(f"{NAME}: {folder} stays while a process of the command holds it", file=out)
        return
    shutil.rmtree(folder, ignore_errors=True)
    os.close(fd)


def lease(config, workspace, out=sys.stderr, sleep=time.sleep, clock=time.monotonic):
    """Takes a free slot, the one `workspace` used last first, waiting up to
    `config.wait` seconds for one, else a private folder."""
    config.root.mkdir(parents=True, exist_ok=True)
    sweep(config.root)
    slots = [config.root / f"slot-{n}" for n in range(1, config.slots + 1)]
    slots.sort(key=lambda folder: owner(folder) != str(workspace))
    deadline = clock() + config.wait
    told = False
    while True:
        for folder in slots:
            folder.mkdir(exist_ok=True)
            held = acquire(folder, slot=True)
            if held is not None:
                return held
        left = deadline - clock()
        if left <= 0:
            break
        if not told:
            print(f"{NAME}: all {config.slots} slots are in use; waiting up to {config.wait:.0f} s", file=out)
            told = True
        sleep(min(5.0, left))
    folder = Path(tempfile.mkdtemp(prefix="private-", dir=config.root))
    print(f"{NAME}: every slot is in use: building in {folder}, which goes when the command ends", file=out)
    return acquire(folder, slot=False)


def run(command, held, workspace, env=os.environ):
    """Runs `command` with CARGO_TARGET_DIR in the held folder and returns its
    exit status. The command inherits the lock, so the folder stays held while
    any process of it lives, even if this one is killed."""
    child_env = dict(env, CARGO_TARGET_DIR=str(held.target), CB_TARGET_SLOT=held.folder.name)
    child_env.update(CB_TARGET_SLOT_WORKTREE=str(workspace), CB_TARGET_SLOT_TOKEN=held.token)
    try:
        child = subprocess.Popen(command, env=child_env, pass_fds=(held.fd,))
    except OSError as error:
        print(f"{NAME}: cannot run {command[0]}: {error}", file=sys.stderr)
        return 127
    # The terminal sends Ctrl-C to the command too; a termination sent to this
    # process alone is passed on.
    signal.signal(signal.SIGINT, lambda *_: None)
    for number in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(number, lambda sig, _: child.send_signal(sig))
    code = child.wait()
    return code if code >= 0 else 128 - code


def status(config, out=sys.stdout):
    """Prints each slot and private folder: whether a command holds it, its size
    and the worktree that used it last."""
    folders = sorted(config.root.glob("slot-*")) + sorted(config.root.glob("private-*"))
    if not folders:
        print(f"no slots in {config.root}", file=out)
    for folder in folders:
        try:
            fd = take(folder / "lock")
        except OSError:
            fd = None
        state = "free" if fd is not None else "in use"
        if fd is not None:
            os.close(fd)
        size = disk_usage(folder / "target")
        print(f"{folder.name:<12} {state:<7} {gigabytes(size):>8}  {owner(folder) or '-'}", file=out)
    return 0


def workspace_members(workspace):
    """The members' names, or none when Cargo.toml cannot be read: a slot
    passing to this worktree is then emptied instead."""
    try:
        return members(workspace)
    except (OSError, tomllib.TOMLDecodeError):
        return []


def main(argv):
    if argv[:1] == ["--status"]:
        return status(settings())
    cwd = None
    if argv[:1] == ["-C"] and len(argv) > 1:
        cwd, argv = argv[1], argv[2:]
    if argv[:1] == ["--"]:
        argv = argv[1:]
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 0 if argv else 2
    workspace = worktree(cwd)
    env = dict(os.environ)
    config = settings(env)
    joined = join(env, workspace, config.root)
    if joined is not None:
        # The command holds the lease through this descriptor for as long as
        # it lives, whatever becomes of the process that took the lease.
        os.set_inheritable(joined, True)
        os.execvp(argv[0], argv)
    # A lease passed on, or one for another worktree, is not this command's
    # to build in.
    for name in ("CARGO_TARGET_DIR", "CB_TARGET_SLOT", "CB_TARGET_SLOT_WORKTREE", "CB_TARGET_SLOT_TOKEN"):
        env.pop(name, None)
    try:
        held = lease(config, workspace)
    except OSError as error:
        print(f"{NAME}: cannot use {config.root} ({error}); building in {workspace}/target", file=sys.stderr)
        os.execvpe(argv[0], argv, env)
    try:
        if held.slot:
            prepare(held, workspace, workspace_members(workspace), config.max_bytes)
        return run(argv, held, workspace, env)
    finally:
        os.close(held.fd)
        if not held.slot:
            remove_private(held.folder)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
