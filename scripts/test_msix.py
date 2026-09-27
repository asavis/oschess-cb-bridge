"""The Store package's version (#112): python3 -m unittest discover -s scripts"""

import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location("msix", os.path.join(HERE, "msix.py"))
msix = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(msix)


def parts(version):
    return tuple(int(part) for part in version.split("."))


class PackageVersion(unittest.TestCase):
    def test_raises_the_first_part_and_leaves_the_fourth_to_the_store(self):
        for app, package in [
            ("0.1.0", "1.1.0.0"),
            ("0.9.12", "1.9.12.0"),
            ("1.0.0", "2.0.0.0"),
            ("65534.65535.65535", "65535.65535.65535.0"),
        ]:
            self.assertEqual(msix.package_version(app), package, app)

    def test_every_later_app_version_packs_higher(self):
        apps = ["0.1.0", "0.1.1", "0.2.0", "0.10.0", "1.0.0", "1.0.1", "1.2.0", "2.0.0"]
        packages = [parts(msix.package_version(app)) for app in apps]
        self.assertEqual(packages, sorted(set(packages)))

    def test_refuses_what_the_store_would(self):
        for app in ["65535.0.0", "0.65536.0", "0.1.65536", "0.1", "0.1.0-beta", "v0.1.0", ""]:
            with self.assertRaises(SystemExit, msg=app):
                msix.package_version(app)

    def test_the_app_s_own_version_packs_within_the_store_s_rules(self):
        version = parts(msix.package_version(msix.app_version()))
        self.assertEqual(len(version), 4)
        self.assertGreater(version[0], 0)
        self.assertEqual(version[3], 0)
        self.assertLessEqual(max(version), 65535)


if __name__ == "__main__":
    unittest.main()
