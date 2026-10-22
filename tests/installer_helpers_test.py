"""Exercise bundled helper commands through the installer's public PATH."""
import os
import subprocess
import unittest

import installer_test


class HelperPathTests(unittest.TestCase):
    def setUp(self):
        self.fixture = installer_test.InstallerTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.fixture.env['PATH'] = (
            str(self.fixture.bin) + os.pathsep + self.fixture.env['PATH']
        )

    def assert_helpers_run(self):
        fixture = self.fixture
        htree = subprocess.run(
            ['htree', '--version'], env=fixture.env, capture_output=True, text=True
        )
        self.assertEqual(htree.returncode, 0, htree.stderr)
        self.assertEqual(htree.stdout.strip(), 'htree 0.2.151')
        helper = subprocess.run(
            ['git-remote-htree'], env=fixture.env, capture_output=True, text=True
        )
        self.assertIn('Usage: git-remote-htree', helper.stderr)
        bundle = (fixture.bin / 'haps').resolve().parent
        for name in ['htree', 'git-remote-htree']:
            self.assertEqual(
                (fixture.bin / name).resolve(), bundle / 'libexec' / name
            )

    def test_fresh_bundle_exposes_both_helpers_on_each_unix_target(self):
        fixture = self.fixture
        for system, machine, target in [
            ('Linux', 'x86_64', 'x86_64-unknown-linux-gnu'),
            ('Linux', 'aarch64', 'aarch64-unknown-linux-gnu'),
            ('Darwin', 'arm64', 'aarch64-apple-darwin'),
            ('Darwin', 'x86_64', 'x86_64-apple-darwin'),
        ]:
            with self.subTest(target=target):
                for name in ['haps', 'htree', 'git-remote-htree']:
                    (fixture.bin / name).unlink(missing_ok=True)
                fixture.env.update(HAPS_TEST_OS=system, HAPS_TEST_ARCH=machine)
                fixture.archive(target, bundled=True)
                result = fixture.run_installer()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assert_helpers_run()

    def test_reinstall_moves_owned_helper_links_with_haps(self):
        fixture = self.fixture
        fixture.archive(bundled=True)
        result = fixture.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        previous = (fixture.bin / 'haps').resolve().parent
        self.assert_helpers_run()
        result = fixture.run_installer('--force')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotEqual(previous, (fixture.bin / 'haps').resolve().parent)
        self.assert_helpers_run()
        self.assertTrue((previous / 'libexec/htree').is_file())
        self.assertTrue((previous / 'libexec/git-remote-htree').is_file())

    def test_separately_installed_helpers_stay_on_path(self):
        fixture = self.fixture
        for name in ['htree', 'git-remote-htree']:
            fixture.tool(name, f'#!/bin/sh\necho separate-{name}\n')
        fixture.archive(bundled=True)
        for args in [(), ('--force',)]:
            result = fixture.run_installer(*args)
            self.assertEqual(result.returncode, 0, result.stderr)
            for name in ['htree', 'git-remote-htree']:
                self.assertFalse((fixture.bin / name).is_symlink())
                result = subprocess.run(
                    [name], env=fixture.env, capture_output=True, text=True
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), f'separate-{name}')
