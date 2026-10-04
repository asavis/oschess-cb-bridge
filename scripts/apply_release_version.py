#!/usr/bin/env python3
"""Apply a reserved release version to the disposable Cargo build workspace."""

import argparse
import re
from pathlib import Path

try:
    from scripts.store_version import version_parts
except ModuleNotFoundError:  # Direct script invocation.
    from store_version import version_parts


def apply_version(root: Path, version: str) -> None:
    version_parts(version)
    manifest = root / "crates/app/Cargo.toml"
    lock = root / "Cargo.lock"
    source = manifest.read_text()
    package = re.search(r"(?ms)^\[package\]\n(.*?)(?=^\[|\Z)", source)
    if package is None:
        raise ValueError("No app package section")
    body, count = re.subn(r'(?m)^version = "[^"\n]+"', f'version = "{version}"', package[1])
    if count != 1:
        raise ValueError("Expected exactly one app package version")
    updated = source[:package.start(1)] + body + source[package.end(1):]
    locked = lock.read_text()
    app = re.compile(r'(?m)(^\[\[package\]\]\nname = "app"\nversion = ")[^"\n]+(")')
    new_lock, count = app.subn(lambda match: match[1] + version + match[2], locked)
    if count != 1:
        raise ValueError("Expected exactly one app entry in Cargo.lock")
    manifest.write_text(updated)
    lock.write_text(new_lock)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    args = parser.parse_args()
    apply_version(Path(__file__).resolve().parents[1], args.version)
