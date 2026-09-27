#!/usr/bin/env python3
"""Packs the Microsoft Store package, oschess-bridge.msix (#112).

    python3 scripts/msix.py --exe target/.../oschess-bridge.exe --out dist/oschess-bridge.msix

The package holds the given oschess-bridge.exe, the manifest
crates/app/msix/AppxManifest.xml with its values filled in, and the images in
crates/app/icons/msix as Assets. It carries no engine: as the installed app,
the Store's copy installs Stockfish only when the user asks (#13).

The identity comes from the variables MSSTORE_IDENTITY_NAME, MSSTORE_PUBLISHER
and MSSTORE_PUBLISHER_DISPLAY_NAME, the values Partner Center shows on the
product's identity page. Without any of them the package gets a test identity,
for installing it by hand (docs/release.md). The version is the app's, with a
fourth part of 0, as the Store requires.

The package is left unsigned: the Store signs it. makeappx.exe comes from
--makeappx or from the newest Windows SDK; --stage-only fills the folder and
stops, on any system.
"""

import argparse
import glob
import os
import re
import shutil
import subprocess
import sys
from typing import NoReturn

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
APP = os.path.join(ROOT, "crates", "app")
TEST_IDENTITY = {
    "NAME": "oschess.bridge.test",
    "PUBLISHER": "CN=oschess bridge test",
    "PUBLISHER_DISPLAY_NAME": "oschess bridge (test)",
}
IDENTITY_VARIABLES = {
    "NAME": "MSSTORE_IDENTITY_NAME",
    "PUBLISHER": "MSSTORE_PUBLISHER",
    "PUBLISHER_DISPLAY_NAME": "MSSTORE_PUBLISHER_DISPLAY_NAME",
}


def fail(message) -> NoReturn:
    sys.exit(f"msix: {message}")


def read(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


def app_version():
    package = read(os.path.join(APP, "Cargo.toml")).split("[package]", 1)[1].split("\n[", 1)[0]
    found = re.search(r'^version = "(\d+)\.(\d+)\.(\d+)"$', package, re.M)
    if not found:
        fail("crates/app/Cargo.toml names no plain x.y.z version")
    return ".".join(found.groups()) + ".0"


def identity():
    given = {key: os.environ.get(variable, "").strip() for key, variable in IDENTITY_VARIABLES.items()}
    if not any(given.values()):
        print("msix: no MSSTORE_* variables, so the package gets the test identity")
        return dict(TEST_IDENTITY)
    missing = [IDENTITY_VARIABLES[key] for key, value in given.items() if not value]
    if missing:
        fail(f"{', '.join(missing)} missing: set all three MSSTORE_* variables or none")
    return given


def stage(folder, exe, values):
    if os.path.exists(folder):
        shutil.rmtree(folder)
    os.makedirs(os.path.join(folder, "Assets"))
    shutil.copy2(exe, os.path.join(folder, "oschess-bridge.exe"))
    manifest = read(os.path.join(APP, "msix", "AppxManifest.xml"))
    for key, value in values.items():
        manifest = manifest.replace(f"@{key}@", escape(value))
    left = re.findall(r"@[A-Z_]+@", manifest)
    if left:
        fail(f"the manifest keeps {', '.join(left)}")
    with open(os.path.join(folder, "AppxManifest.xml"), "w", encoding="utf-8", newline="\r\n") as f:
        f.write(manifest)
    for image in glob.glob(os.path.join(APP, "icons", "msix", "*.png")):
        shutil.copy2(image, os.path.join(folder, "Assets"))


def escape(text):
    return text.replace("&", "&amp;").replace('"', "&quot;").replace("<", "&lt;").replace(">", "&gt;")


def makeappx(given):
    if given:
        return given
    found = sorted(glob.glob(r"C:\Program Files (x86)\Windows Kits\10\bin\10.*\x64\makeappx.exe"))
    if not found:
        fail("no makeappx.exe: install the Windows SDK or pass --makeappx")
    return found[-1]


def main():
    parser = argparse.ArgumentParser(description="Packs the Microsoft Store package, oschess-bridge.msix.")
    parser.add_argument("--exe", required=True, help="the oschess-bridge.exe to pack")
    parser.add_argument("--out", required=True, help="the .msix to write; its folder also gets the staging folder")
    parser.add_argument("--makeappx", help="makeappx.exe, instead of the newest Windows SDK's")
    parser.add_argument("--stage-only", action="store_true", help="fill the staging folder and stop")
    args = parser.parse_args()

    if not os.path.isfile(args.exe):
        fail(f"{args.exe}: no such file")
    out_dir = os.path.dirname(os.path.abspath(args.out))
    values = identity()
    values["VERSION"] = app_version()
    folder = os.path.join(out_dir, "msix-stage")
    stage(folder, args.exe, values)
    print(f"msix: staged {folder} as {values['NAME']} {values['VERSION']}")
    if args.stage_only:
        return
    tool = makeappx(args.makeappx)
    code = subprocess.run([tool, "pack", "/o", "/d", folder, "/p", os.path.abspath(args.out)]).returncode
    if code != 0:
        fail(f"makeappx exited with {code}")
    print(f"msix: wrote {args.out}")


if __name__ == "__main__":
    main()
