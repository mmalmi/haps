"""Bounded, explicit source coverage reads pinned objects, never host paths."""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location('audit_adapter', Path(__file__).resolve().parents[1] / 'src/integrations/audit.py')
ADAPTER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ADAPTER)


class Snapshot(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.git('init')
        self.git('config', 'user.name', 'Fixture')
        self.git('config', 'user.email', 'fixture@example.invalid')

    def git(self, *args):
        return subprocess.check_output(['git', '-C', str(self.root), *args], stderr=subprocess.DEVNULL)

    def commit(self):
        self.git('add', '.')
        self.git('commit', '-m', 'Pinned source')

    def test_large_project_supports_selected_reads_and_reports_omissions(self):
        (self.root / 'README.md').write_text('Build instructions')
        (self.root / 'implementation.txt').write_text('security relevant code')
        (self.root / 'large.dat').write_bytes(b'x' * (ADAPTER.MAX_SOURCE + 1))
        self.commit()
        snapshot = ADAPTER.SourceSnapshot(self.root, {})
        self.assertEqual(snapshot.coverage()['tracked_files'], 3)
        self.assertNotIn('implementation.txt', snapshot.files)
        snapshot.read('implementation.txt')
        snapshot.read('large.dat')
        self.assertEqual(snapshot.coverage()['reviewed_files'], 2)
        self.assertEqual(snapshot.coverage()['omitted_files'], 1)
        self.assertIn('budget', snapshot.files['large.dat']['omitted'])
        with self.assertRaisesRegex(ValueError, 'outside'):
            snapshot.read('../private')

    def test_omitted_binary_blobs_still_consume_the_read_budget(self):
        (self.root / 'README.one').write_bytes(b'\0' * (ADAPTER.MAX_SOURCE // 2))
        (self.root / 'README.two').write_bytes(b'\0' * (ADAPTER.MAX_SOURCE // 2))
        (self.root / 'code.txt').write_text('not yet inspected')
        self.commit()
        snapshot = ADAPTER.SourceSnapshot(self.root, {})
        with patch.object(snapshot, 'git', wraps=snapshot.git) as git:
            snapshot.read('README.two')
            snapshot.read('code.txt')
        self.assertEqual(git.call_count, 1, 'binary files must consume the raw read budget')
        self.assertIn('budget', snapshot.files['code.txt']['omitted'])
        self.assertEqual(snapshot.coverage()['reviewed_bytes'], 0)

    def test_reads_committed_bytes_and_never_follows_host_symlink(self):
        (self.root / 'README.md').write_text('committed source')
        if os.name != 'nt':
            (self.root / 'link').symlink_to('/etc/passwd')
        self.commit()
        (self.root / 'README.md').write_text('unreviewed working copy')
        snapshot = ADAPTER.SourceSnapshot(self.root, {})
        self.assertEqual(snapshot.files['README.md']['content'], 'committed source')
        if os.name != 'nt':
            self.assertIn('not followed', snapshot.files['link']['omitted'])

    def test_multi_round_reads_and_unicode_coverage_note_stay_bounded(self):
        (self.root / 'README.md').write_text('Instructions')
        (self.root / 'code.txt').write_text('review this')
        (self.root / 'large.dat').write_bytes(b'x' * (ADAPTER.MAX_SOURCE + 1))
        self.commit()
        reports = [dict(verdict='read', note='Need implementation', files=['code.txt']),
                   dict(verdict='pass', note='🪻' * 1000, files=[])]
        with patch.object(ADAPTER, 'review_once', side_effect=reports):
            result = ADAPTER.review(self.root, {}, 'codex')
        self.assertEqual(result['coverage']['reviewed_files'], 2)
        self.assertLess(len(result['note'].encode()), 4096)
        self.assertIn('1 not inspected', result['note'])


if __name__ == '__main__':
    unittest.main()
