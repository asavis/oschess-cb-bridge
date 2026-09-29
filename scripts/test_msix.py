"""The Store package's version and packing (#112): python3 -m unittest discover -s scripts"""

import importlib.util
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location("msix", os.path.join(HERE, "msix.py"))
msix = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(msix)


def parts(version):
    return tuple(int(part) for part in version.split("."))


def write(path, text):
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)


class AppVersion(unittest.TestCase):
    """The version Cargo reads for crates/app, which release.yml checks the tag
    against, whatever the manifest's layout."""

    def version_of(self, app, workspace="[workspace]\n"):
        with tempfile.TemporaryDirectory() as root:
            os.makedirs(os.path.join(root, "crates", "app"))
            write(os.path.join(root, "Cargo.toml"), workspace)
            write(os.path.join(root, "crates", "app", "Cargo.toml"), app)
            return msix.app_version(root)

    def test_reads_the_package_s_own_version(self):
        app = (
            '[package]\nname = "app"\nversion = "1.2.3" # the tag names it\nedition.workspace = true\n\n'
            '[dependencies]\nbridge = { version = "0.1.0", path = "../bridge" }\n'
        )
        self.assertEqual(self.version_of(app), "1.2.3")

    def test_reads_the_workspace_s_version_when_the_package_inherits_it(self):
        app = '[package]\nname = "app"\nversion.workspace = true\n'
        workspace = '[workspace]\nmembers = ["crates/*"]\n\n[workspace.package]\nversion = "2.0.1"\n'
        self.assertEqual(self.version_of(app, workspace), "2.0.1")

    def test_refuses_a_manifest_without_a_version(self):
        for app in ['[package]\nname = "app"\n', '[package]\nname = "app"\nversion.workspace = true\n']:
            with self.assertRaises(SystemExit, msg=app):
                self.version_of(app)

    @unittest.skipUnless(shutil.which("cargo"), "needs cargo")
    def test_reads_the_version_release_yml_takes_from_cargo(self):
        # release.yml: version=$(cargo pkgid -p app | sed 's/.*[#@]//')
        pkgid = subprocess.run(
            ["cargo", "pkgid", "-p", "app", "--locked", "--offline"],
            cwd=msix.ROOT,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
        self.assertEqual(msix.app_version(), re.sub(r".*[#@]", "", pkgid))


class PackageVersion(unittest.TestCase):
    def test_keeps_the_app_s_version_and_leaves_the_fourth_part_to_the_store(self):
        for app, package in [
            ("1.0.0", "1.0.0.0"),
            ("1.2.3", "1.2.3.0"),
            ("65535.65535.65535", "65535.65535.65535.0"),
        ]:
            self.assertEqual(msix.package_version(app), package, app)

    def test_every_later_app_version_packs_higher(self):
        apps = ["1.0.0", "1.0.1", "1.2.0", "1.10.0", "2.0.0"]
        packages = [parts(msix.package_version(app)) for app in apps]
        self.assertEqual(packages, sorted(set(packages)))

    def test_refuses_what_the_store_would(self):
        for app in ["0.1.0", "0.2.0", "65536.0.0", "1.65536.0", "1.0.65536", "1.0", "1.0.0-beta", "v1.0.0", ""]:
            with self.assertRaises(SystemExit, msg=app):
                msix.package_version(app)

    def test_the_app_s_own_version_packs_within_the_store_s_rules(self):
        version = parts(msix.package_version(msix.app_version()))
        self.assertEqual(len(version), 4)
        self.assertGreater(version[0], 0)
        self.assertEqual(version[3], 0)
        self.assertLessEqual(max(version), 65535)


class Packing(unittest.TestCase):
    def pack(self, *extra):
        """Runs the script with the SDK tools stubbed out; returns the commands
        it ran and the staging folder."""
        work = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, work)
        exe = os.path.join(work, "oschess-bridge.exe")
        with open(exe, "wb") as f:
            f.write(b"MZ")
        out = os.path.join(work, "dist", "oschess-bridge.msix")
        os.makedirs(os.path.dirname(out))
        argv = ["msix.py", "--exe", exe, "--out", out, "--makepri", "makepri", "--makeappx", "makeappx", *extra]
        ran = []

        def run(command):
            ran.append(command)
            return mock.Mock(returncode=0)

        with (
            mock.patch.object(sys, "argv", argv),
            mock.patch.dict(os.environ, {name: "" for name in msix.IDENTITY_VARIABLES.values()}),
            mock.patch.object(msix.subprocess, "run", run),
            mock.patch("builtins.print"),
        ):
            msix.main()
        return ran, os.path.join(os.path.dirname(out), "msix-stage")

    def test_indexes_the_images_into_the_package_before_packing_it(self):
        ran, folder = self.pack()
        steps = [command[:2] for command in ran]
        self.assertEqual(steps, [["makepri", "createconfig"], ["makepri", "new"], ["makeappx", "pack"]])
        createconfig, new, pack = ran
        config = new[new.index("/cf") + 1]
        self.assertEqual(createconfig[createconfig.index("/cf") + 1], config)
        self.assertFalse(config.startswith(folder + os.sep), "the configuration stays out of the package")
        self.assertEqual(new[new.index("/pr") + 1], folder)
        self.assertEqual(new[new.index("/of") + 1], os.path.join(folder, "resources.pri"))
        self.assertEqual(pack[pack.index("/d") + 1], folder)

    def test_a_staged_folder_is_indexed_too(self):
        ran, _ = self.pack("--stage-only")
        self.assertEqual([command[:2] for command in ran], [["makepri", "createconfig"], ["makepri", "new"]])


if __name__ == "__main__":
    unittest.main()
