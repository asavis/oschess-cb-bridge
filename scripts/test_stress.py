"""The stress runner's parsing and report (#217): python3 -m unittest discover -s scripts"""

import importlib.util
import json
import os
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


class FailedTests(unittest.TestCase):
    """The failed tests in libtest's output."""

    def test_names_each_failed_test_and_nothing_else(self):
        output = "\n".join(
            [
                "running 3 tests",
                "test a_connection_serves_several_requests ... ok",
                "test busy_and_misdirected_answers_carry_cors ... FAILED",
                "test http::tests::parses_requests ... FAILED",
                "2026-09-30T11:19:00Z the bridge cannot start: test ... FAILED later",
                "test result: FAILED. 1 passed; 2 failed; 0 ignored",
            ]
        )
        self.assertEqual(
            stress.failed_tests(output), ["busy_and_misdirected_answers_carry_cors", "http::tests::parses_requests"]
        )
        self.assertEqual(stress.failed_tests("test result: ok. 3 passed"), [])


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


if __name__ == "__main__":
    unittest.main()
