"""Calendar allocation and atomic ownership, with no network or credentials."""

import copy
import json
import unittest
from datetime import date

from scripts import store_version as versions


REPO = "example/app"
SHA = "a" * 40
CONFIG = {"major": 1, "platforms": {"bridge": {
    "minimum_version": "1.4.0", "minimum_build": 33, "legacy_tag_prefix": "v"}}}


class FakeGitHub:
    def __init__(self):
        self.references = {}
        self.tags = {}
        self.lose_reply = False
        self.before_claim = None
        self.ref_reads_fail = False

    def refs(self, repository, prefix):
        return [copy.deepcopy(value) for name, value in self.references.items()
                if name.startswith("refs/tags/" + prefix)]

    def call(self, method, path, payload=None):
        path = path.removeprefix(f"repos/{REPO}/git/")
        if method == "POST" and path == "tags":
            sha = f"{len(self.tags) + 1:040x}"
            self.tags[sha] = {"message": payload["message"], "object": {
                "type": payload["type"], "sha": payload["object"]}}
            return {"sha": sha}
        if method == "POST" and path == "refs":
            if self.before_claim:
                callback, self.before_claim = self.before_claim, None
                callback()
            if payload["ref"] in self.references:
                raise versions.VersionError("422 already exists")
            self.references[payload["ref"]] = {"ref": payload["ref"],
                "object": {"type": "tag", "sha": payload["sha"]}}
            if self.lose_reply:
                raise versions.VersionError("connection lost after commit")
            return copy.deepcopy(self.references[payload["ref"]])
        if method == "GET" and path.startswith("tags/"):
            return copy.deepcopy(self.tags[path.removeprefix("tags/")])
        if method == "GET" and path.startswith("ref/"):
            if self.ref_reads_fail:
                raise versions.VersionError("read unavailable")
            return copy.deepcopy(self.references["refs/" + path.removeprefix("ref/")])
        raise AssertionError((method, path, payload))


class StoreVersionTests(unittest.TestCase):
    def allocate(self, api, day=date(2027, 1, 1), config=None, run="42"):
        return versions.reserve(api, REPO, "bridge", config or CONFIG, SHA, day, run, "1")

    def test_owner_examples_and_decimal_components(self):
        for day, previous, expected in [
            (date(2027, 1, 1), "1.4.0", "1.2701.101"),
            (date(2027, 1, 1), "1.2701.101", "1.2701.102"),
            (date(2027, 1, 1), "1.2701.109", "1.2701.110"),
            (date(2027, 1, 10), "1.2701.999", "1.2701.1001"),
            (date(2026, 10, 4), "1.4.0", "1.2610.401"),
            (date(2026, 10, 14), "1.2610.499", "1.2610.1401"),
            (date(2027, 2, 1), "1.2701.3199", "1.2702.101"),
            (date(2028, 1, 1), "1.2712.3199", "1.2801.101"),
            (date(2028, 2, 29), "1.2802.2899", "1.2802.2901"),
        ]:
            with self.subTest(expected=expected):
                actual = versions.next_version(1, day, previous)
                self.assertEqual(actual, expected)
                self.assertTrue(all(str(int(part)) == part for part in actual.split(".")))

    def test_daily_exhaustion_never_changes_major(self):
        self.assertEqual(versions.next_version(1, date(2027, 1, 1), "1.2701.198"), "1.2701.199")
        with self.assertRaisesRegex(versions.VersionError, "99 build"):
            versions.next_version(1, date(2027, 1, 1), "1.2701.199")

    def test_backwards_clock_and_major_are_refused(self):
        for previous in ("1.2701.201", "1.2801.101", "2.2601.101"):
            with self.subTest(previous=previous), self.assertRaises(versions.VersionError):
                versions.next_version(1, date(2027, 1, 1), previous)

    def test_only_owner_config_can_raise_major(self):
        self.assertEqual(versions.next_version(2, date(2027, 1, 1), "1.2712.3199"), "2.2701.101")
        for major in (True, 0, -1, 65536, "1"):
            with self.subTest(major=major), self.assertRaises(versions.VersionError):
                versions.next_version(major, date(2027, 1, 1), "1.4.0")

    def test_year_width_never_silently_wraps(self):
        for year in (1999, 2100):
            with self.assertRaises(versions.VersionError):
                versions.next_version(1, date(year, 1, 1), "1.4.0")

    def test_invalid_versions_fail_closed(self):
        for value in ("01.2701.101", "1.2701.0101", "1.2", "1.2.3.0", "1.2.3-rc1", "1.65536.1"):
            with self.subTest(value=value), self.assertRaises(versions.VersionError):
                versions.version_parts(value)

    def test_reservation_consumes_version_before_build_and_reruns_get_new_numbers(self):
        api = FakeGitHub()
        one = self.allocate(api)
        two = self.allocate(api)
        self.assertEqual((one["version"], one["build"]), ("1.2701.101", 34))
        self.assertEqual((two["version"], two["build"]), ("1.2701.102", 35))
        self.assertEqual(len(api.references), 2)
        self.assertEqual(two["sha"], SHA)

    def test_concurrent_claimant_gets_a_distinct_later_identity(self):
        api = FakeGitHub()
        winner = []
        api.before_claim = lambda: winner.append(self.allocate(api, run="43"))
        loser = self.allocate(api)
        self.assertEqual(winner[0]["build"], 34)
        self.assertEqual((loser["build"], loser["version"]), (35, "1.2701.102"))

    def test_unknown_write_outcome_is_proved_without_a_second_reservation(self):
        api = FakeGitHub()
        api.lose_reply = True
        self.assertEqual(self.allocate(api)["build"], 34)
        self.assertEqual(len(api.references), 1)

    def test_unreadable_write_outcome_never_claims_success(self):
        api = FakeGitHub()
        api.ref_reads_fail = True
        with self.assertRaisesRegex(versions.VersionError, "read unavailable"):
            self.allocate(api)
        self.assertEqual(len(api.references), 1)

    def test_history_keeps_build_increasing_when_day_or_major_changes(self):
        api = FakeGitHub()
        self.allocate(api)
        changed = copy.deepcopy(CONFIG)
        changed["major"] = 2
        later = self.allocate(api, date(2027, 1, 2), changed)
        self.assertEqual((later["version"], later["build"]), ("2.2701.201", 35))

    def test_historical_release_tags_are_a_migration_floor(self):
        api = FakeGitHub()
        name = "refs/tags/v1.2701.110"
        api.references[name] = {"ref": name, "object": {"type": "commit", "sha": SHA}}
        self.assertEqual(self.allocate(api)["version"], "1.2701.111")

    def test_corrupt_history_is_not_treated_as_empty(self):
        api = FakeGitHub()
        self.allocate(api)
        tag = next(iter(api.tags.values()))
        data = json.loads(tag["message"])
        data["platform"] = "different-app"
        tag["message"] = json.dumps(data)
        with self.assertRaisesRegex(versions.VersionError, "Invalid reservation"):
            self.allocate(api)



class AppliedVersionTests(unittest.TestCase):
    def test_reserved_version_reaches_cargo_and_msix_without_changing_dependencies(self):
        import tempfile
        import tomllib
        from pathlib import Path
        from scripts.apply_release_version import apply_version
        from scripts.msix import app_version, package_version
        root = Path(__file__).resolve().parents[1]
        with tempfile.TemporaryDirectory() as folder:
            work = Path(folder)
            (work / 'crates/app').mkdir(parents=True)
            original = (root / 'Cargo.lock').read_text()
            (work / 'Cargo.lock').write_text(original)
            (work / 'crates/app/Cargo.toml').write_text((root / 'crates/app/Cargo.toml').read_text())
            apply_version(work, '1.2701.101')
            self.assertEqual(app_version(work), '1.2701.101')
            self.assertEqual(package_version(app_version(work)), '1.2701.101.0')
            before = tomllib.loads(original)['package']
            after = tomllib.loads((work / 'Cargo.lock').read_text())['package']
            self.assertEqual([x for x in before if x['name'] != 'app'],
                             [x for x in after if x['name'] != 'app'])
            self.assertEqual(next(x['version'] for x in after if x['name'] == 'app'), '1.2701.101')

    def test_release_jobs_share_one_version_and_keep_owner_publication_boundary(self):
        from pathlib import Path
        source = (Path(__file__).resolve().parents[1] / '.github/workflows/release.yml').read_text()
        self.assertIn('on:\n  workflow_dispatch:', source)
        self.assertNotIn("tags: ['v*']", source)
        self.assertIn('test "$GITHUB_REF" = refs/heads/main', source)
        self.assertLess(source.index('CI passed for the source commit'), source.index('Reserve the store version'))
        self.assertEqual(source.count('python scripts/apply_release_version.py "$VERSION"'), 3)
        self.assertIn('releases/download/v$VERSION/', source)
        self.assertIn('gh release create "v$VERSION"', source)
        self.assertIn('--draft --target "$GITHUB_SHA"', source)
        self.assertNotIn('--clobber', source)


if __name__ == "__main__":
    unittest.main()
