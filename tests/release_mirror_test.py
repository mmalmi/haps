import contextlib
import hashlib
import io
import json
from pathlib import Path
import runpy
import subprocess
import sys
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
from release import TARGETS


class ReleaseHistoryGuard(unittest.TestCase):
    def run_mirror(self):
        with patch.object(sys, 'argv', ['publish-hashtree-release.py', '--tag', 'v0.1.11',
                                       '--expected-root', 'a' * 64 + ':' + 'b' * 64]), \
                contextlib.redirect_stdout(io.StringIO()):
            runpy.run_path(str(ROOT / 'scripts/publish-hashtree-release.py'))

    def test_old_helper_is_rejected_before_any_upload(self):
        with patch('subprocess.check_output', return_value='Usage: htree release publish') as read, \
                patch('subprocess.run') as mutate:
            with self.assertRaisesRegex(SystemExit, 'protect existing releases'):
                self.run_mirror()
        read.assert_called_once_with(['htree', 'release', 'publish', '--help'], text=True)
        mutate.assert_not_called()

    def test_verified_archive_publication_carries_the_expected_root(self):
        commit = 'c' * 40
        installer = b'#!/bin/sh\n'
        published = []

        def read(args, **kwargs):
            if args == ['htree', 'release', 'publish', '--help']:
                return 'Options: --expected-root <CID>'
            if args[:2] == ['git', 'rev-parse']:
                return commit + '\n'
            if args[:2] == ['git', 'show']:
                return installer
            if args[:2] == ['htree', 'add']:
                stage = Path(args[2])
                self.assertEqual((stage / 'version.txt').read_text(), 'v0.1.11\n')
                return '  url:   nhash1verified\n'
            self.fail(f'Unexpected read: {args}')

        def run(args, **kwargs):
            if args[:3] == ['gh', 'release', 'download']:
                directory = Path(args[args.index('--dir') + 1])
                assets = []
                for target in TARGETS:
                    suffix = '.zip' if 'windows' in target else '.tar.gz'
                    name = f'haps-v0.1.11-{target}{suffix}'
                    data = target.encode()
                    (directory / name).write_bytes(data)
                    assets.append({'name': name, 'size': len(data),
                                   'sha256': hashlib.sha256(data).hexdigest()})
                manifest = {'tag': 'v0.1.11', 'version': '0.1.11',
                            'commit': commit, 'assets': assets}
                (directory / 'release.json').write_text(json.dumps(manifest))
                (directory / 'SHA256SUMS').write_text(''.join(
                    f'{asset["sha256"]}  {asset["name"]}\n' for asset in assets))
                (directory / 'install.sh').write_bytes(installer)
            elif args[:3] == ['htree', 'release', 'publish']:
                published.append(args)
            else:
                self.fail(f'Unexpected mutation: {args}')
            return subprocess.CompletedProcess(args, 0)

        with patch('subprocess.check_output', side_effect=read), \
                patch('subprocess.run', side_effect=run):
            self.run_mirror()
        self.assertEqual(published, [['htree', 'release', 'publish', 'releases/haps',
                                     'v0.1.11', 'nhash1verified', '--expected-root',
                                     'a' * 64 + ':' + 'b' * 64]])


if __name__ == '__main__':
    unittest.main()
