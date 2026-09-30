#!/usr/bin/env python3
"""Runs the workspace's tests over and over on a loaded machine, to find the
tests that fail only there, and to accept their fixes (#217, #233). Linux
only.

    python3 scripts/stress.py --runs 300 --bin api --bin explorer_background
    python3 scripts/stress.py --runs 50 --jobs 5 --fail-fast

The tests are built once, as `cargo test` builds them, and their binaries run
directly, each in its package's folder as `cargo test` runs it; doctests are
left out. A run is every chosen binary once, in turn. `--jobs` makes that
many runs at a time: every test process names its fixtures after its own
process id, so runs at the same time never share one. Arguments after `--`
go to every binary, such as a test name filter.

The load is the script's own. The tests run at nice 19 on `--cpus`, beside
`--hogs` busy loops at nice `--hog-nice` on the same CPUs, so a thread of a
test waits for its turn as on a busy workstation. That is how #217's failures
were found: a test thread stalled for seconds between two steps, a kernel
that queued connections out of order, the rest of a request arriving just as
the bridge refused it. The defaults are the load that found them: a quarter
of the machine's CPUs, three loops on each, all at nice 19. Other programs'
load adds to it, so `--hog-nice 0` makes up for a quiet machine. Before the
load starts, one run without it times the tests, and the summary compares
the loaded runs with it; on a busy machine that run can fail too, and its
failures count with the others.

Each failed run keeps its output in `--out`, target/stress/<UTC time> by
default, and the summary counts the failures by test. The exit status is 1
when a run failed.
"""

import argparse
import json
import os
import re
import signal
import statistics
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HOG = "while :; do :; done"
FAILED_TEST = re.compile(r"^test (\S+) \.\.\. FAILED$", re.MULTILINE)


@dataclass(frozen=True)
class Binary:
    """A test binary: its target's name, its label in the summary, its path,
    and the folder `cargo test` runs it in."""

    name: str
    label: str
    exe: str
    cwd: str


@dataclass
class Outcome:
    """What one binary did in one run: the tests that failed, and `problem`
    when it failed some other way (a crash, a time-out)."""

    binary: Binary
    seconds: float
    failed: list = field(default_factory=list)
    problem: str = ""
    output: str = ""

    @property
    def ok(self):
        return not self.failed and not self.problem


def binaries(cargo_messages):
    """The test binaries in `cargo test --no-run --message-format=json` output,
    in build order. A target's label is its name, with its kind when that is
    not an integration test: `api`, `bridge [lib]`, `oschess-bridge [bin]`."""
    found = []
    for line in cargo_messages.splitlines():
        if not line.startswith("{"):
            continue
        message = json.loads(line)
        if message.get("reason") != "compiler-artifact" or not message.get("executable"):
            continue
        if not message.get("profile", {}).get("test"):
            continue
        target = message["target"]
        kinds = "+".join(target["kind"])
        label = target["name"] if kinds == "test" else f"{target['name']} [{kinds}]"
        found.append(Binary(target["name"], label, message["executable"], str(Path(message["manifest_path"]).parent)))
    return found


def failed_tests(output):
    """The names of the tests libtest reports as failed in `output`."""
    return FAILED_TEST.findall(output)


def parse_cpus(text):
    """The CPU numbers of a `taskset -c` list such as `0-3,8`."""
    cpus = set()
    for part in text.split(","):
        first, dash, last = part.strip().partition("-")
        if not first.isdigit() or (dash and not last.isdigit()) or int(last or first) < int(first):
            raise ValueError(f"{text!r} is not a CPU list such as 0-3,8")
        cpus.update(range(int(first), int(last or first) + 1))
    return cpus


def format_cpus(cpus):
    """`cpus` as a `taskset -c` list, runs of numbers as ranges."""
    ordered, parts = sorted(cpus), []
    start = prev = None
    for cpu in ordered + [None]:
        if cpu is not None and prev is not None and cpu == prev + 1:
            prev = cpu
            continue
        if start is not None:
            parts.append(str(start) if start == prev else f"{start}-{prev}")
        start = prev = cpu
    return ",".join(parts)


def default_cpus(allowed):
    """A quarter of the CPUs this process may use, at least two (or all of
    them, when there are fewer)."""
    ordered = sorted(allowed)
    return set(ordered[: max(min(2, len(ordered)), len(ordered) // 4)])


def summary(outcomes_by_run, loads):
    """The report after the runs: how many failed, how long a run took, and
    the failures by test, most frequent first. Run 0 is the run without the
    script's load: it times the others, and its failures count with theirs."""
    loaded = {run: outcomes for run, outcomes in outcomes_by_run.items() if run != 0}
    failed = sum(1 for outcomes in loaded.values() if not all(o.ok for o in outcomes))
    share = f" ({100 * failed / len(loaded):.1f} %)" if loaded else ""
    lines = [f"{len(loaded)} runs under load, {failed} failed{share}"]
    unloaded = outcomes_by_run.get(0)
    unloaded_seconds = sum(o.seconds for o in unloaded) if unloaded else 0
    if unloaded:
        verdict = "ok" if all(o.ok for o in unloaded) else "FAILED"
        lines.append(f"the run without the load: {verdict}, {unloaded_seconds:.1f} s")
    if loaded:
        seconds = [sum(o.seconds for o in outcomes) for outcomes in loaded.values()]
        median = statistics.median(seconds)
        timing = f"a run under load took {median:.1f} s (median), {max(seconds):.1f} s at most"
        if unloaded_seconds:
            timing += f", {median / unloaded_seconds:.1f} times the run without it"
        lines.append(timing)
    if loads:
        lines.append(f"load average {min(loads):.1f} to {max(loads):.1f}")
    counts = {}
    for outcomes in outcomes_by_run.values():
        for o in outcomes:
            for name in o.failed or ([o.problem] if o.problem else []):
                key = f"{o.binary.label}: {name}"
                counts[key] = counts.get(key, 0) + 1
    if counts:
        lines.append("failures by test:")
        lines += [f"  {n:4}  {key}" for key, n in sorted(counts.items(), key=lambda item: (-item[1], item[0]))]
    return "\n".join(lines)


def build():
    """Builds the tests as `cargo test` does and returns their binaries."""
    done = subprocess.run(
        ["cargo", "test", "--no-run", "--message-format=json-render-diagnostics"],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        text=True,
    )
    if done.returncode != 0:
        sys.exit("cargo test --no-run failed")
    return binaries(done.stdout)


class Aborted(Exception):
    """The script is ending early and starts nothing more."""


class Live:
    """The process groups started and not yet ended: each test binary with
    the processes it starts, and each busy loop. Each runs in a session of
    its own, which a Ctrl-C in the terminal does not reach, so the script
    kills them itself when it ends early, and starts none after that."""

    lock = threading.Lock()
    groups = set()
    closed = False

    @classmethod
    def start(cls, args, **kwargs):
        with cls.lock:
            if cls.closed:
                raise Aborted
            process = subprocess.Popen(args, start_new_session=True, **kwargs)
            cls.groups.add(process.pid)
        return process

    @classmethod
    def kill(cls, process):
        with cls.lock:
            cls.groups.discard(process.pid)
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    @classmethod
    def ended(cls, process):
        with cls.lock:
            cls.groups.discard(process.pid)

    @classmethod
    def kill_all(cls):
        with cls.lock:
            cls.closed = True
            for group in cls.groups:
                try:
                    os.killpg(group, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            cls.groups.clear()


def run_binary(binary, prefix, libtest_args, timeout):
    """Runs `binary` once under `prefix` (taskset and nice, or nothing): its
    outcome. A binary still running after `timeout` seconds is killed with
    every process it started."""
    started = time.monotonic()
    process = Live.start(
        [*prefix, binary.exe, *libtest_args],
        cwd=binary.cwd,
        env=dict(os.environ, CARGO_MANIFEST_DIR=binary.cwd),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        errors="replace",
    )
    try:
        output, _ = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        Live.kill(process)
        output, _ = process.communicate()
        return Outcome(binary, time.monotonic() - started, problem=f"still running after {timeout} s", output=output)
    Live.ended(process)
    seconds = time.monotonic() - started
    failed = failed_tests(output)
    problem = "" if process.returncode == 0 or failed else f"exited with {process.returncode}"
    return Outcome(binary, seconds, failed, problem, output)


class Hogs:
    """Busy loops on `cpus` at nice `nice`, stopped when the block ends."""

    def __init__(self, count, cpus, nice):
        self.args = ["taskset", "-c", cpus, "nice", "-n", str(nice), "sh", "-c", HOG]
        self.count = count
        self.processes = []

    def __enter__(self):
        for _ in range(self.count):
            self.processes.append(Live.start(self.args))
        return self

    def __exit__(self, *_):
        for process in self.processes:
            Live.kill(process)
            process.wait()


def write_log(out, name, outcomes):
    """Keeps a failed run's output: one file, `name`.log, with every
    binary's."""
    path = out / f"{name}.log"
    with open(path, "w", encoding="utf-8") as f:
        for o in outcomes:
            f.write(f"===== {o.binary.label} ({o.seconds:.1f} s){': ' + o.problem if o.problem else ''}\n")
            f.write(o.output)
    return path


def interrupted(*_):
    """Ends the script on a plain kill as Ctrl-C does, so that nothing it
    started outlives it."""
    raise KeyboardInterrupt


def arguments(argv):
    parser = argparse.ArgumentParser(description=(__doc__ or "").split("\n\n")[0])
    parser.add_argument("--runs", type=int, default=100, help="how many runs (default 100)")
    parser.add_argument("--jobs", type=int, default=1, help="runs at the same time (default 1)")
    parser.add_argument(
        "--bin", action="append", default=[], help="a test binary by its target name, such as api (default: all)"
    )
    parser.add_argument(
        "--cpus", help="the CPUs of the tests and the loops, as taskset -c takes them (default: a quarter of them)"
    )
    parser.add_argument("--hogs", type=int, help="busy loops on those CPUs (default: three per CPU)")
    parser.add_argument("--hog-nice", type=int, default=19, help="the loops' nice value (default 19, the tests')")
    parser.add_argument("--timeout", type=int, default=1800, help="seconds a binary may run (default 1800)")
    parser.add_argument("--fail-fast", action="store_true", help="start no run after one failed")
    parser.add_argument("--out", type=Path, help="where failed runs' output goes (default target/stress/<UTC time>)")
    parser.add_argument("libtest", nargs="*", help="after --: arguments for every test binary")
    args = parser.parse_args(argv)
    if args.runs < 1 or args.jobs < 1:
        parser.error("--runs and --jobs take a number above 0")
    return args


def main(argv=None):
    args = arguments(argv)
    if not sys.platform.startswith("linux"):
        sys.exit("stress.py needs Linux: taskset and nice")
    allowed = os.sched_getaffinity(0)
    try:
        cpus = parse_cpus(args.cpus) if args.cpus else default_cpus(allowed)
    except ValueError as e:
        sys.exit(str(e))
    if not cpus <= allowed:
        sys.exit(f"CPUs {format_cpus(cpus - allowed)} are not available to this process")
    cpu_list = format_cpus(cpus)
    hogs = 3 * len(cpus) if args.hogs is None else args.hogs
    out = args.out or ROOT / "target" / "stress" / time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    out.mkdir(parents=True, exist_ok=True)

    chosen = build()
    if args.bin:
        unknown = set(args.bin) - {b.name for b in chosen}
        if unknown:
            sys.exit(f"no test binary named {', '.join(sorted(unknown))}")
        chosen = [b for b in chosen if b.name in args.bin]
    print(f"{len(chosen)} test binaries: {', '.join(b.label for b in chosen)}", flush=True)

    lock = threading.Lock()
    results, loads, next_run = {}, [], iter(range(1, args.runs + 1))
    # `stop`: start no more runs; `aborted`: the script is ending early, and
    # the runs it cut short are no one's failures.
    stop, aborted = threading.Event(), threading.Event()

    def report(run, outcomes):
        """Records run `run`, prints its line, and keeps its output when it
        failed."""
        load = os.getloadavg()[0]
        line = f"run {run} {{}}, {sum(o.seconds for o in outcomes):.1f} s, load {load:.1f}"
        with lock:
            results[run] = outcomes
            if run:
                loads.append(load)
            if all(o.ok for o in outcomes):
                print(line.format("ok"), flush=True)
                return
            failures = [f"{o.binary.label}: {', '.join(o.failed) or o.problem}" for o in outcomes if not o.ok]
            print(f"{line.format('FAILED')}: {'; '.join(failures)}", flush=True)
            print(f"  {write_log(out, f'run-{run}', outcomes)}", flush=True)
            if args.fail_fast:
                stop.set()

    print("run 0: without the load", flush=True)
    report(0, [run_binary(b, [], args.libtest, args.timeout) for b in chosen])

    prefix = ["taskset", "-c", cpu_list, "nice", "-n", "19"]
    print(f"load: the tests at nice 19 on CPUs {cpu_list}, beside {hogs} loops at nice {args.hog_nice}", flush=True)

    def job():
        while not stop.is_set():
            with lock:
                run = next(next_run, None)
            if run is None:
                return
            try:
                outcomes = [run_binary(b, prefix, args.libtest, args.timeout) for b in chosen]
            except Aborted:
                return
            if aborted.is_set():
                return
            report(run, outcomes)

    signal.signal(signal.SIGTERM, interrupted)
    stopped = False
    try:
        with Hogs(hogs, cpu_list, args.hog_nice), ThreadPoolExecutor(args.jobs) as pool:
            futures = [pool.submit(job) for _ in range(args.jobs)]
            try:
                for future in futures:
                    future.result()
            except BaseException:
                stop.set()
                aborted.set()
                Live.kill_all()
                raise
    except KeyboardInterrupt:
        stopped = True
        print("stopped")

    print(summary(dict(sorted(results.items())), loads))
    print(f"output of failed runs: {out}")
    if stopped:
        return 130
    return 0 if all(o.ok for outcomes in results.values() for o in outcomes) else 1


if __name__ == "__main__":
    sys.exit(main())
