import argparse
import importlib.util
import os
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('public_source', ROOT / 'scripts/verify-public-source.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class PublicSourceGate(unittest.TestCase):
    def test_cold_fetch_cannot_use_local_objects_or_daemon(self):
        revision = 'a' * 40
        with patch.dict(os.environ, HTREE_CONFIG_DIR='/publisher', HTREE_DATA_DIR='/cached',
                        NOSTR_RELAYS='ws://localhost:9999', GIT_ALTERNATE_OBJECT_DIRECTORIES='/objects'), \
                patch.object(module.subprocess, 'run') as clone, \
                patch.object(module.subprocess, 'check_output', side_effect=[revision, 'false', 'objects']) as read:
            module.verify('htree://npub1fixture/source', revision, ROOT / 'helpers')
        args, options = clone.call_args
        self.assertIn('--no-local', args[0])
        self.assertIn('--no-checkout', args[0])
        self.assertEqual(options['timeout'], 300)
        env = options['env']
        self.assertNotEqual(env['HTREE_CONFIG_DIR'], '/publisher')
        self.assertNotEqual(env['HTREE_DATA_DIR'], '/cached')
        self.assertNotIn('GIT_ALTERNATE_OBJECT_DIRECTORIES', env)
        self.assertNotIn('NOSTR_RELAYS', env)
        self.assertEqual(env['HTREE_PREFER_LOCAL_DAEMON'], 'false')
        self.assertEqual(env['HTREE_LOCAL_DAEMON_ONLY'], 'false')
        self.assertEqual(read.call_args.args[0][-5:], ['rev-list', '--objects', '--missing=error', revision, '--'])

    def test_larger_clone_deadline_preserves_history_checks(self):
        revision = 'a' * 40
        with patch.object(module.subprocess, 'run') as clone, \
                patch.object(module.subprocess, 'check_output', side_effect=[revision, 'false', 'objects']) as read:
            module.verify('htree://npub1fixture/source', revision, ROOT, clone_timeout=1800)
        self.assertEqual(clone.call_args.kwargs['timeout'], 1800)
        self.assertEqual(read.call_count, 3)
        self.assertTrue(all(call.kwargs['timeout'] == 120 for call in read.call_args_list))
        self.assertEqual(read.call_args.args[0][-5:], ['rev-list', '--objects', '--missing=error', revision, '--'])

    def test_invalid_deadlines_stop_before_fetch(self):
        for timeout in (0, -1, 1801, '1.5', 'inf'):
            with self.subTest(timeout=timeout), patch.object(module.subprocess, 'run') as clone:
                with self.assertRaises(argparse.ArgumentTypeError):
                    module.verify('htree://npub1fixture/source', 'a' * 40, ROOT, clone_timeout=timeout)
                clone.assert_not_called()

    def test_missing_parents_and_shallow_history_stop_release(self):
        revision = 'a' * 40
        with patch.object(module.subprocess, 'run'), \
                patch.object(module.subprocess, 'check_output', side_effect=[revision, 'true']):
            with self.assertRaisesRegex(ValueError, 'incomplete history'):
                module.verify('htree://npub1fixture/source', revision, ROOT)
        with patch.object(module.subprocess, 'run'), \
                patch.object(module.subprocess, 'check_output', side_effect=[
                    revision, 'false', subprocess.CalledProcessError(128, 'git rev-list')]):
            with self.assertRaises(subprocess.CalledProcessError):
                module.verify('htree://npub1fixture/source', revision, ROOT)

    def test_workflow_checks_source_before_build_and_publication(self):
        workflow = (ROOT / '.github/workflows/release.yml').read_text()
        source_job, build_job = workflow.split('  build:', 1)
        self.assertIn('scripts/verify-public-source.py', source_job)
        self.assertIn('needs: source', build_job)
        self.assertIn('needs: [source, build]', build_job)


if __name__ == '__main__':
    unittest.main()
