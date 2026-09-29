"""The Store package's version and packing (#112): python3 -m unittest discover -s scripts"""

import importlib.util
import os
import shutil
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
