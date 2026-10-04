#!/usr/bin/env python3
"""Reserve calendar store versions in append-only GitHub annotated tag refs.

Each invocation reserves a NEW build, including a rerun. Only the successful
atomic ref creation owns a number; an ambiguous response is read back before
doing anything else. No Git history, release or tag is overwritten.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import uuid
from datetime import date, datetime, timezone
from pathlib import Path
from typing import Any


class VersionError(Exception):
    """The allocator cannot prove a safe next identity."""


def version_parts(value: str) -> tuple[int, int, int]:
    if not isinstance(value, str) or not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value):
        raise VersionError(f"Invalid three-part version: {value!r}")
    parts = tuple(map(int, value.split(".")))
    if not 1 <= parts[0] <= 65535 or max(parts) > 65535:
        raise VersionError("Version is outside the Store package limits")
    return parts


def next_version(major: int, today: date, previous: str) -> str:
    if type(major) is not int or not 1 <= major <= 65535:
        raise VersionError("major must be an owner-selected integer from 1 to 65535")
    if not 2000 <= today.year <= 2099:
        raise VersionError("The two-digit-year policy requires review outside 2000–2099")
    old = version_parts(previous)
    month = (today.year - 2000) * 100 + today.month
    sequence = 1
    if old[:2] == (major, month) and old[2] // 100 == today.day:
        sequence = old[2] % 100 + 1
    if sequence > 99:
        raise VersionError("All 99 build numbers for this UTC day are reserved; wait for the next day")
    parts = (major, month, today.day * 100 + sequence)
    if parts <= old:
        raise VersionError("Refusing a non-increasing version; check the clock and owner-selected major")
    return ".".join(map(str, parts))


class GitHub:
    def call(self, method: str, path: str, payload: dict | None = None) -> Any:
        args = ["gh", "api", "--method", method, path]
        if payload is not None:
            args += ["--input", "-"]
        try:
            result = subprocess.run(args, input=json.dumps(payload) if payload is not None else None,
                                    capture_output=True, text=True, timeout=60, check=False)
        except (OSError, subprocess.TimeoutExpired) as error:
            raise VersionError(f"GitHub {method} {path} did not complete") from error
        if result.returncode:
            raise VersionError(f"GitHub {method} {path} failed: {result.stderr.strip()[:300]}")
        try:
            return json.loads(result.stdout)
        except ValueError as error:
            raise VersionError(f"GitHub {method} {path} returned invalid JSON") from error

    def refs(self, repository: str, prefix: str) -> list[dict]:
        # matching-refs returns the complete array (unlike the paginated tags API).
        result = self.call("GET", f"repos/{repository}/git/matching-refs/tags/{prefix}")
        if not isinstance(result, list):
            raise VersionError("GitHub returned an invalid reference listing")
        return result


def reserve(api: GitHub, repository: str, platform: str, config: dict, sha: str,
            today: date, run_id: str, attempt: str) -> dict:
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise VersionError("Invalid repository")
    if not re.fullmatch(r"[a-z][a-z0-9-]*", platform) or platform not in config["platforms"]:
        raise VersionError("Unknown platform")
    if not re.fullmatch(r"[0-9a-f]{40}", sha) or not run_id.isdecimal() or not attempt.isdecimal():
        raise VersionError("Missing Actions source/run identity")
    settings = config["platforms"][platform]
    minimum = settings["minimum_version"]
    version_parts(minimum)
    floor = settings["minimum_build"]
    if type(floor) is not int or not 0 <= floor < 2_100_000_000:
        raise VersionError("Invalid technical build floor")
    legacy = settings["legacy_tag_prefix"]
    if not re.fullmatch(r"[a-z-]*v", legacy):
        raise VersionError("Invalid historical tag prefix")
    for ref in api.refs(repository, legacy):
        name = ref["ref"].removeprefix(f"refs/tags/{legacy}")
        if re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", name):
            # Historical pre-1.0 tags are below every store version.
            if name.startswith("0."):
                continue
            minimum = max(minimum, name, key=version_parts)
    prefix = f"store-build/{platform}/"
    for _ in range(20):
        refs = api.refs(repository, prefix)
        latest = None
        for ref in refs:
            suffix = ref["ref"].removeprefix(f"refs/tags/{prefix}")
            if not re.fullmatch(r"[1-9][0-9]*", suffix) or ref["object"]["type"] != "tag":
                raise VersionError("Malformed reservation ref; refusing to reset its counter")
            if latest is None or int(suffix) > latest[0]:
                latest = (int(suffix), ref["object"]["sha"])
        previous = minimum
        build = floor + 1
        if latest:
            tag = api.call("GET", f"repos/{repository}/git/tags/{latest[1]}")
            try:
                record = json.loads(tag["message"])
                if (record["schema"] != 1 or record["platform"] != platform
                        or record["repository"] != repository or record["build"] != latest[0]
                        or tag["object"]["type"] != "commit" or tag["object"]["sha"] != record["sha"]):
                    raise ValueError("reservation identity mismatch")
                previous = max(previous, record["version"], key=version_parts)
            except (KeyError, TypeError, ValueError) as error:
                raise VersionError("Invalid reservation metadata; refusing to reset its counter") from error
            build = max(floor, latest[0]) + 1
        if build > 2_100_000_000:
            raise VersionError("Technical build counter is exhausted")
        version = next_version(config["major"], today, previous)
        record = {"schema": 1, "repository": repository, "platform": platform,
                  "sha": sha, "date": today.isoformat(), "version": version, "build": build,
                  "run_id": run_id, "run_attempt": attempt, "reservation": str(uuid.uuid4())}
        name = f"{prefix}{build}"
        created = api.call("POST", f"repos/{repository}/git/tags",
                           {"tag": name, "message": json.dumps(record, sort_keys=True),
                            "object": sha, "type": "commit"})
        object_sha = created["sha"]
        write_error = None
        try:
            api.call("POST", f"repos/{repository}/git/refs",
                     {"ref": f"refs/tags/{name}", "sha": object_sha})
        except VersionError as error:
            write_error = error
        # Both success and failure require proof. An unreadable or absent ref
        # is terminal; the next invocation can consume a new number safely.
        found = api.call("GET", f"repos/{repository}/git/ref/tags/{name}")
        if found["object"]["type"] == "tag" and found["object"]["sha"] == object_sha:
            return record
        if write_error is None:
            raise VersionError("Reservation changed after a successful write")
        # A concurrent claimant owns this number. Read its record on the next
        # iteration and choose a larger version and technical build number.
    raise VersionError("Too many concurrent reservation conflicts; retry the build")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--config", type=Path, default=Path("store-version.json"))
    parser.add_argument("--output", type=Path, default=Path("store-release.json"))
    args = parser.parse_args()
    try:
        if os.environ.get("GITHUB_ACTIONS") != "true":
            raise VersionError("Reserve versions only in the release workflow")
        config = json.loads(args.config.read_text())
        # Preserve the workflow run counter as an additional migration floor.
        # The durable ledger remains authoritative after workflow renames.
        settings = config["platforms"][args.platform]
        settings["minimum_build"] = max(settings["minimum_build"], int(os.environ["GITHUB_RUN_NUMBER"]) - 1)
        record = reserve(GitHub(), os.environ["GITHUB_REPOSITORY"], args.platform,
                         config, os.environ["GITHUB_SHA"],
                         datetime.now(timezone.utc).date(), os.environ["GITHUB_RUN_ID"],
                         os.environ["GITHUB_RUN_ATTEMPT"])
        args.output.write_text(json.dumps(record, indent=2) + "\n")
        with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
            output.write(f'version={record["version"]}\nbuild={record["build"]}\n')
        with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as summary:
            summary.write(f'### Store build\n{args.platform}: **{record["version"]}**, '
                          f'build **{record["build"]}**, source `{record["sha"]}`. '
                          'Reserved permanently; a new build gets a new number.\n')
        print(f'{args.platform} {record["version"]} build {record["build"]}')
        return 0
    except (VersionError, KeyError, TypeError, ValueError, OSError) as error:
        print(f"store_version: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
