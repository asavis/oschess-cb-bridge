#!/usr/bin/env python3
"""Packs the Microsoft Store package, oschess-bridge.msix (#112).

    python3 scripts/msix.py --exe exe/oschess-bridge.exe --out dist/oschess-bridge.msix

The package holds the given tray app as oschess-bridge.exe: the release's
oschess-bridge.exe, which the Tauri CLI renames, or a cargo build's
target/release/oschess-bridge-app.exe, never the console bridge that cargo
names oschess-bridge.exe. With it go the manifest
crates/app/msix/AppxManifest.xml with its values filled in, and the images in
crates/app/icons/msix as Assets. With --stockfish, the official Stockfish
archive the release downloaded, it also carries the pinned x64 Stockfish under
engines\\stockfish-<version> with its licence and a notice (#338): the Store signs
the whole package, so Smart App Control lets the engine run, which it does not
for the unsigned download the installed app would fetch on request (#13).

The identity comes from the variables MSSTORE_IDENTITY_NAME, MSSTORE_PUBLISHER
and MSSTORE_PUBLISHER_DISPLAY_NAME, the values Partner Center shows on the
product's identity page. Without any of them the package gets a test identity,
for installing it by hand (docs/release.md). The version comes from the app's
x.y.z by package_version.

Windows finds the images named by size and theme (targetsize-24_altform-
unplated, targetsize-24_altform-lightunplated and the like) only through the
package's resources.pri, which makepri.exe writes. Without it Windows takes
Square44x44Logo.png for the taskbar and the Start menu and lays it on a plate
of the accent colour, which shows in the tile's rounded corners.

The package is left unsigned: the Store signs it. makeappx.exe and makepri.exe
come from --makeappx and --makepri or from the newest Windows SDK;
--stage-only fills and indexes the folder and stops before packing it.
"""

import argparse
import glob
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tomllib
import zipfile
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


def app_version(root=ROOT):
    """The app's version, x.y.z, as Cargo reads it, so the one release.yml
    takes from `cargo pkgid -p app` and checks the tag against: package.version
    in crates/app/Cargo.toml, or the workspace's when the package inherits it."""
    version = load_toml(os.path.join(root, "crates", "app", "Cargo.toml")).get("package", {}).get("version")
    if version == {"workspace": True}:
        workspace = load_toml(os.path.join(root, "Cargo.toml")).get("workspace", {})
        version = workspace.get("package", {}).get("version")
    if not isinstance(version, str):
        fail("crates/app/Cargo.toml names no version, of its own or from the workspace")
    return version


def load_toml(path):
    with open(path, "rb") as f:
        return tomllib.load(f)


def package_version(version):
    """The package version for the app's x.y.z: the same, with a fourth part
    of 0, so the Store shows the version the app does (1.0.0 packs as
    1.0.0.0). The Store needs a first part above 0, every part at most 65535
    and a fourth part of 0, so a 0.x version cannot be packed."""
    found = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", version)
    if not found:
        fail(f"{version!r} is not a plain x.y.z version")
    parts = tuple(int(part) for part in found.groups()) + (0,)
    if parts[0] == 0:
        fail(f"{version}: the Store takes no package version whose first part is 0")
    if max(parts) > 65535:
        fail(f"{version} does not fit a package version, whose parts go up to 65535")
    return ".".join(str(part) for part in parts)


def identity():
    given = {key: os.environ.get(variable, "").strip() for key, variable in IDENTITY_VARIABLES.items()}
    if not any(given.values()):
        print("msix: no MSSTORE_* variables, so the package gets the test identity")
        return dict(TEST_IDENTITY)
    missing = [IDENTITY_VARIABLES[key] for key, value in given.items() if not value]
    if missing:
        fail(f"{', '.join(missing)} missing: set all three MSSTORE_* variables or none")
    return given


def pinned_stockfish(root=ROOT):
    """The x64 build crates/bridge/src/stockfish.rs pins: its version, release
    asset, size, SHA-256 and executable name. One pin serves the download the
    app offers and the build the package ships."""
    text = read(os.path.join(root, "crates", "bridge", "src", "stockfish.rs"))
    found = re.search(
        r'version: Cow::Borrowed\("([^"]+)"\),\s*arch: Arch::X86_64,\s*asset: "([^"]+)",\s*size: ([\d_]+),\s*'
        r'sha256: Cow::Borrowed\("([0-9a-f]{64})"\),\s*exe: "([^"]+)"',
        text,
    )
    if not found:
        fail("stockfish.rs pins no x86-64 build in the shape msix.py reads")
    version, asset, size, sha256, exe = found.groups()
    return {"version": version, "asset": asset, "size": int(size.replace("_", "")), "sha256": sha256, "exe": exe}


def notice(pin, bridge_version):
    return (
        f"Stockfish {pin['version']} is part of this package.\r\n\r\n"
        "Stockfish is free software under the GNU General Public License, version 3 or\r\n"
        "later; its licence is in Copying.txt beside it. The bridge runs it as a separate\r\n"
        "program and talks to it over the UCI protocol.\r\n\r\n"
        "Source code: the Stockfish project publishes it at\r\n"
        f"https://github.com/official-stockfish/Stockfish/tree/sf_{pin['version']}\r\n"
        "The same source is attached to the release of this version of the bridge,\r\n"
        f"stockfish-sf_{pin['version']}-source.tar.gz at\r\n"
        f"https://github.com/asavis/oschess-cb-bridge/releases/tag/v{bridge_version}\r\n"
        "and for three years after the last release that includes this build, anyone who\r\n"
        "asks for it there gets the complete source of Stockfish as shipped, at no\r\n"
        "charge beyond the cost of the copy.\r\n"
    )


def bundle_stockfish(folder, archive, pin, bridge_version):
    """Puts the pinned Stockfish from the official `archive` under the staged
    folder: refused unless the archive is the pinned one, byte for byte."""
    if os.path.getsize(archive) != pin["size"]:
        fail(f"{archive}: {os.path.getsize(archive)} bytes, the pin says {pin['size']}")
    with open(archive, "rb") as f:
        digest = hashlib.sha256(f.read()).hexdigest()
    if digest != pin["sha256"]:
        fail(f"{archive}: SHA-256 {digest}, the pin says {pin['sha256']}")
    target = os.path.join(folder, "engines", f"stockfish-{pin['version']}")
    os.makedirs(target)
    with zipfile.ZipFile(archive) as z:
        for name in (pin["exe"], "Copying.txt"):
            try:
                data = z.read(f"stockfish/{name}")
            except KeyError:
                fail(f"{archive} has no stockfish/{name}")
            with open(os.path.join(target, name), "wb") as f:
                f.write(data)
    with open(os.path.join(target, "NOTICE.txt"), "w", encoding="utf-8", newline="") as f:
        f.write(notice(pin, bridge_version))


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


def sdk_tool(name, given):
    if given:
        return given
    found = sorted(glob.glob(rf"C:\Program Files (x86)\Windows Kits\10\bin\10.*\x64\{name}.exe"))
    if not found:
        fail(f"no {name}.exe: install the Windows SDK or pass --{name}")
    return found[-1]


def run(command, what):
    code = subprocess.run(command).returncode
    if code != 0:
        fail(f"{what} exited with {code}")


def index(folder, tool, work):
    """Writes the folder's resources.pri, so that Windows finds the images by
    their size and theme qualifiers. The configuration stays outside the
    folder, which the package holds as it is."""
    config = os.path.join(work, "priconfig.xml")
    run([tool, "createconfig", "/cf", config, "/dq", "uk-UA_en-US", "/pv", "10.0.0", "/o"], "makepri createconfig")
    manifest = os.path.join(folder, "AppxManifest.xml")
    pri = os.path.join(folder, "resources.pri")
    run([tool, "new", "/pr", folder, "/cf", config, "/mn", manifest, "/of", pri, "/o"], "makepri new")


def main():
    parser = argparse.ArgumentParser(description="Packs the Microsoft Store package, oschess-bridge.msix.")
    parser.add_argument(
        "--exe",
        required=True,
        help="the tray app, packed as oschess-bridge.exe: the release's oschess-bridge.exe"
        " or a cargo build's oschess-bridge-app.exe",
    )
    parser.add_argument("--out", required=True, help="the .msix to write; its folder also gets the staging folder")
    parser.add_argument(
        "--stockfish",
        help="the official Stockfish archive, the pinned stockfish-windows-x86-64-universal.zip, to ship in the package",
    )
    parser.add_argument("--makeappx", help="makeappx.exe, instead of the newest Windows SDK's")
    parser.add_argument("--makepri", help="makepri.exe, instead of the newest Windows SDK's")
    parser.add_argument("--stage-only", action="store_true", help="fill and index the staging folder and stop")
    args = parser.parse_args()

    if not os.path.isfile(args.exe):
        fail(f"{args.exe}: no such file")
    out_dir = os.path.dirname(os.path.abspath(args.out))
    values = identity()
    values["VERSION"] = package_version(app_version())
    folder = os.path.join(out_dir, "msix-stage")
    stage(folder, args.exe, values)
    if args.stockfish:
        bundle_stockfish(folder, args.stockfish, pinned_stockfish(), app_version())
    index(folder, sdk_tool("makepri", args.makepri), out_dir)
    print(f"msix: staged {folder} as {values['NAME']} {values['VERSION']}")
    if args.stage_only:
        return
    run([sdk_tool("makeappx", args.makeappx), "pack", "/o", "/d", folder, "/p", os.path.abspath(args.out)], "makeappx")
    print(f"msix: wrote {args.out}")


if __name__ == "__main__":
    main()
