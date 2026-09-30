"""The stress runner's parsing and report (#217): python3 -m unittest discover -s scripts"""

import importlib.util
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location("stress", os.path.join(HERE, "stress.py"))
stress = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(stress)


def artifact(name, kinds, executable, test=True, manifest="/repo/crates/bridge/Cargo.toml"):
    return json.dumps(
        {
            "reason": "compiler-artifact",
            "target": {"name": name, "kind": kinds},
            "profile": {"test": test},
            "executable": executable,
            "manifest_path": manifest,
        }
    )


class Binaries(unittest.TestCase):
    """The test binaries in cargo's JSON messages."""

    def test_takes_test_executables_and_labels_them_by_kind(self):
        messages = "\n".join(
            [
                "   Compiling bridge v0.1.0",
                artifact("bridge", ["lib"], "/t/deps/bridge-1"),
                artifact("api", ["test"], "/t/deps/api-2"),
                artifact("oschess-bridge", ["bin"], "/t/deps/oschess_bridge-3"),
                artifact("bridge", ["lib"], None, test=False),
                artifact("cbtool", ["bin"], "/t/debug/cbtool", test=False),
                json.dumps({"reason": "build-finished", "success": True}),
            ]
        )
        found = stress.binaries(messages)
        self.assertEqual([b.label for b in found], ["bridge [lib]", "api", "oschess-bridge [bin]"])
        self.assertEqual([b.name for b in found], ["bridge", "api", "oschess-bridge"])
        self.assertEqual(found[1].exe, "/t/deps/api-2")
        self.assertEqual(found[1].cwd, "/repo/crates/bridge")


PRETTY = """
running 3 tests
test passes ... ok
test deliberate_failure ... FAILED
test second_failure ... FAILED

failures:

---- deliberate_failure stdout ----
failures:

thread 'deliberate_failure' panicked at p.rs:2:58:
boom

---- second_failure stdout ----

thread 'second_failure' panicked at p.rs:3:31:
assertion `left == right` failed


failures:
    deliberate_failure
    second_failure

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
"""

QUIET = """
running 3 tests
second_failure --- FAILED
. 2/3
deliberate_failure --- FAILED

failures:

---- second_failure stdout ----

thread 'second_failure' panicked at p.rs:3:31:
assertion `left == right` failed


failures:
    deliberate_failure
    second_failure

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
"""

NOCAPTURE = """
running 3 tests
test deliberate_failure ... failures:

thread 'deliberate_failure' panicked at p.rs:2:58:
boom
FAILED
test passes ... ok
test second_failure ...
thread 'second_failure' panicked at p.rs:3:31:
assertion `left == right` failed
FAILED

failures:

failures:
    deliberate_failure
    second_failure

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
"""


class FailedTests(unittest.TestCase):
    """The failed tests in libtest's output, as a rustc --test binary prints
    it (thread ids left out)."""

    def test_names_the_failed_tests_in_every_output_mode(self):
        for mode, output in (("pretty", PRETTY), ("--quiet", QUIET), ("--nocapture", NOCAPTURE)):
            with self.subTest(mode):
                self.assertEqual(stress.failed_tests(output), ["deliberate_failure", "second_failure"])

    def test_names_nothing_when_every_test_passed(self):
        self.assertEqual(stress.failed_tests("running 1 test\ntest passes ... ok\n\ntest result: ok. 1 passed"), [])


class Cpus(unittest.TestCase):
    """CPU lists as taskset -c takes them."""

    def test_reads_and_writes_ranges(self):
        self.assertEqual(stress.parse_cpus("0-3,8"), {0, 1, 2, 3, 8})
        self.assertEqual(stress.format_cpus({8, 0, 1, 2, 3, 10, 11}), "0-3,8,10-11")
        self.assertEqual(stress.format_cpus({5}), "5")
        for bad in ("", "a", "1-", "-2", "1-2-3", "5-2"):
            with self.assertRaises(ValueError, msg=bad):
                stress.parse_cpus(bad)

    def test_takes_a_quarter_by_default_and_at_least_two(self):
        self.assertEqual(stress.default_cpus(set(range(32))), set(range(8)))
        self.assertEqual(stress.default_cpus({0, 2, 4, 6, 8, 10, 12, 14}), {0, 2})
        self.assertEqual(stress.default_cpus({3}), {3})


class Slowdown(unittest.TestCase):
    """How many times run 0 the loaded runs took, by their median."""

    def test_divides_the_loaded_median_by_run_0(self):
        api = stress.Binary("api", "api", "/t/api", "/repo")
        runs = {0: [stress.Outcome(api, 2.0)], 1: [stress.Outcome(api, 6.0)], 2: [stress.Outcome(api, 12.0)]}
        self.assertEqual(stress.slowdown(runs), 4.5)

    def test_is_unknown_without_run_0_or_a_loaded_run(self):
        api = stress.Binary("api", "api", "/t/api", "/repo")
        self.assertIsNone(stress.slowdown({1: [stress.Outcome(api, 6.0)]}))
        self.assertIsNone(stress.slowdown({0: [stress.Outcome(api, 2.0)]}))


class Summary(unittest.TestCase):
    """The report after the runs."""

    def test_counts_failed_runs_and_failures_by_test(self):
        api = stress.Binary("api", "api", "/t/api", "/repo")
        lib = stress.Binary("bridge", "bridge [lib]", "/t/bridge", "/repo")
        runs = {
            0: [stress.Outcome(api, 1.0, failed=["busy"]), stress.Outcome(lib, 1.0)],
            1: [stress.Outcome(api, 5.0), stress.Outcome(lib, 1.0)],
            2: [stress.Outcome(api, 9.0, failed=["busy"]), stress.Outcome(lib, 1.0)],
            3: [
                stress.Outcome(api, 7.0, failed=["busy", "reset"]),
                stress.Outcome(lib, 2.0, problem="exited with -11"),
            ],
            4: [stress.Outcome(api, 5.0), stress.Outcome(lib, 1.0)],
        }
        report = stress.summary(runs, [40.0, 90.5]).splitlines()
        self.assertEqual(
            report,
            [
                "4 runs under load, 2 failed (50.0 %)",
                "the run without the load: FAILED, 2.0 s",
                "a run under load took 7.5 s (median), 10.0 s at most, 3.8 times the run without it",
                "load average 40.0 to 90.5",
                "failures by test:",
                "     3  api: busy",
                "     1  api: reset",
                "     1  bridge [lib]: exited with -11",
            ],
        )

    def test_a_clean_report_lists_no_failures(self):
        api = stress.Binary("api", "api", "/t/api", "/repo")
        report = stress.summary({0: [stress.Outcome(api, 1.5)], 1: [stress.Outcome(api, 3.0)]}, [12.0])
        self.assertEqual(
            report.splitlines(),
            [
                "1 runs under load, 0 failed (0.0 %)",
                "the run without the load: ok, 1.5 s",
                "a run under load took 3.0 s (median), 3.0 s at most, 2.0 times the run without it",
                "load average 12.0 to 12.0",
            ],
        )

# Runs stress.py's main with a fake build: `build` holds the build open with a
# `sleep`; otherwise the build gives one test binary, the script `fake-test`.
# `kill` sends the script a SIGTERM just before its first process group kill,
# `submit` just after it hands its first run to a worker.
DRIVER = """
import importlib.util, os, signal, sys
spec = importlib.util.spec_from_file_location("stress", sys.argv[1])
stress = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stress)
mode, tmp = sys.argv[2], sys.argv[3]

if mode == "kill":
    real_killpg, fired = os.killpg, []

    def killpg(group, sig):
        if not fired:
            fired.append(group)
            os.kill(os.getpid(), signal.SIGTERM)
        real_killpg(group, sig)

    os.killpg = killpg

if mode == "submit":

    class Pool(stress.ThreadPoolExecutor):
        def submit(self, *args, **kwargs):
            future = super().submit(*args, **kwargs)
            os.kill(os.getpid(), signal.SIGTERM)
            return future

    stress.ThreadPoolExecutor = Pool


def build():
    if mode == "build":
        sleep = stress.Live.start(["sleep", "300"])
        with open(os.path.join(tmp, "pids"), "a") as f:
            f.write(f"{sleep.pid}\\n")
        sleep.wait()
    return [stress.Binary("fake", "fake", os.path.join(tmp, "fake-test"), tmp)]


stress.build = build
sys.exit(stress.main(["--out", os.path.join(tmp, "out"), *sys.argv[4:]]))
"""

# The fake test binary: it notes its process id and a child's, whose output
# goes elsewhere. The first run (run 0) ends at once when `$1` is `quick`;
# the others sleep until killed, or exit at once with 101 when it is `exit`;
# every run passes at once when it is `fast`.
FAKE_TEST = """#!/bin/sh
sleep 300 >/dev/null 2>&1 &
echo "$$ $!" >> pids
if [ "$MODE" = exit ]; then exit 101; fi
if [ "$MODE" = fast ]; then exit 0; fi
if [ "$MODE" = quick ] && [ ! -e first ]; then touch first; exit 0; fi
exec sleep 300
"""


def alive(pid):
    """Whether process `pid` runs: it exists and is not a zombie."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            stat = f.read()
    except FileNotFoundError:
        return False
    return stat[stat.rfind(")") + 2] != "Z"


def children(pid):
    """The processes whose parent is `pid`, from the kernel's per-thread lists."""
    found = []
    for task in os.listdir(f"/proc/{pid}/task"):
        with open(f"/proc/{pid}/task/{task}/children") as f:
            found += [int(p) for p in f.read().split()]
    return found


@unittest.skipUnless(sys.platform.startswith("linux") and shutil.which("taskset"), "Linux with taskset")
class FakeRun(unittest.TestCase):
    """The runner run in a child process with a fake build and test binary."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        with open(os.path.join(self.tmp, "driver.py"), "w") as f:
            f.write(DRIVER)
        fake = os.path.join(self.tmp, "fake-test")
        with open(fake, "w") as f:
            f.write(FAKE_TEST)
        os.chmod(fake, 0o755)
        self.cpu = str(min(os.sched_getaffinity(0)))

    def start(self, mode, *args, env_mode=""):
        log = open(os.path.join(self.tmp, "driver.log"), "w")
        self.addCleanup(log.close)
        driver = subprocess.Popen(
            [sys.executable, os.path.join(self.tmp, "driver.py"), os.path.join(HERE, "stress.py"), mode, self.tmp]
            + list(args),
            stdout=log,
            stderr=subprocess.STDOUT,
            env=dict(os.environ, MODE=env_mode),
        )
        self.addCleanup(lambda: driver.poll() is None and driver.kill())
        return driver

    def noted(self, count):
        """The process ids noted in `pids` once there are `count` lines."""
        path = os.path.join(self.tmp, "pids")
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if os.path.exists(path):
                with open(path) as f:
                    lines = f.read().splitlines()
                if len(lines) >= count:
                    return [int(pid) for line in lines for pid in line.split()]
            time.sleep(0.05)
        self.fail(f"fewer than {count} lines in pids")

    def assert_all_end(self, pids):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and any(alive(pid) for pid in pids):
            time.sleep(0.05)
        self.assertEqual([pid for pid in pids if alive(pid)], [])

    def stop(self, driver, sig):
        driver.send_signal(sig)
        self.assertEqual(driver.wait(timeout=60), 130)


class Lifecycle(FakeRun):
    """Nothing the runner starts outlives it: a signal in any phase kills the
    build, the tests with their children and the busy loops, and a test
    binary's leftover children are killed when it exits (#233)."""

    def test_a_signal_during_the_build(self):
        driver = self.start("build", "--hogs", "0")
        pids = self.noted(1)
        self.stop(driver, signal.SIGTERM)
        self.assert_all_end(pids)

    def test_a_signal_during_the_run_without_the_load(self):
        driver = self.start("run", "--hogs", "0")
        pids = self.noted(1)
        self.stop(driver, signal.SIGINT)
        self.assert_all_end(pids)

    def test_a_signal_under_load(self):
        driver = self.start("run", "--runs", "5", "--jobs", "2", "--cpus", self.cpu, "--hogs", "2", env_mode="quick")
        pids = self.noted(3)
        hogs = [pid for pid in children(driver.pid) if alive(pid) and pid not in pids]
        self.assertGreaterEqual(len(hogs), 2, "the busy loops were not found")
        self.stop(driver, signal.SIGTERM)
        self.assert_all_end(pids + hogs)
        with open(os.path.join(self.tmp, "driver.log")) as f:
            self.assertNotIn("FAILED", f.read(), "a run the script cut short counted as failed")

    def test_a_signal_as_a_group_is_killed(self):
        driver = self.start("kill", "--hogs", "0", env_mode="exit")
        self.assertEqual(driver.wait(timeout=60), 130)
        self.assert_all_end(self.noted(1))

    def test_a_signal_as_the_runs_are_handed_out(self):
        driver = self.start("submit", "--runs", "3", "--jobs", "1", "--cpus", self.cpu, "--hogs", "0", env_mode="quick")
        self.assertEqual(driver.wait(timeout=60), 130)
        pids = self.noted(1)
        self.assertLessEqual(len(pids), 4, "runs started after the signal")
        self.assert_all_end(pids)

    def test_a_binary_s_leftover_children(self):
        driver = self.start("run", "--runs", "1", "--hogs", "0", env_mode="exit")
        self.assertEqual(driver.wait(timeout=60), 1)
        self.assert_all_end(self.noted(2))


class MinSlowdown(FakeRun):
    """`--min-slowdown` voids a campaign whose load did not slow the tests
    (#235): exit status 2, with the reason, when every run passed."""

    def test_a_campaign_below_the_slowdown_is_void(self):
        driver = self.start("run", "--runs", "2", "--hogs", "0", "--min-slowdown", "1000", env_mode="fast")
        self.assertEqual(driver.wait(timeout=60), 2)
        with open(os.path.join(self.tmp, "driver.log")) as f:
            self.assertIn("void: the loaded runs took", f.read())

    def test_a_campaign_at_the_slowdown_passes(self):
        driver = self.start("run", "--runs", "2", "--hogs", "0", "--min-slowdown", "0.01", env_mode="fast")
        self.assertEqual(driver.wait(timeout=60), 0)


if __name__ == "__main__":
    unittest.main()
