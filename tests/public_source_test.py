import argparse
import importlib.util
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

try:
    import tomllib
except ImportError:
    tomllib = None

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('public_source', ROOT / 'scripts/verify-public-source.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class PublicSourceGate(unittest.TestCase):
    def test_default_fetch_does_not_override_server_configuration(self):
        revision = 'a' * 40

        def inspect_clone(*args, **options):
            self.assertFalse((Path(options['env']['HTREE_CONFIG_DIR']) / 'config.toml').exists())
            self.assertFalse(Path(options['env']['HTREE_DATA_DIR']).exists())

        with patch.object(module.subprocess, 'run', side_effect=inspect_clone), \
                patch.object(module.subprocess, 'check_output', side_effect=[revision, 'false', 'objects']):
            module.verify('htree://npub1fixture/source', revision, ROOT)

    @unittest.skipUnless(tomllib, 'Configuration validation needs Python 3.11+')
    def test_explicit_servers_use_only_fresh_isolated_configuration(self):
        revision = 'a' * 40
        for selected in (['https://cdn.iris.to'], ['https://upload.iris.to'],
                         ['https://upload.iris.to', 'https://cdn.iris.to']):
            with self.subTest(servers=selected), tempfile.TemporaryDirectory() as publisher:
                original = Path(publisher) / 'config.toml'
                original.write_text('[blossom]\nread_servers = ["https://publisher.invalid"]\n')
                before = original.read_bytes()

                def inspect_clone(*args, **options):
                    env = options['env']
                    config = Path(env['HTREE_CONFIG_DIR']) / 'config.toml'
                    self.assertNotEqual(config, original)
                    settings = tomllib.loads(config.read_text())
                    self.assertEqual(settings, {'blossom': {
                        'servers': [], 'read_servers': sorted(selected), 'write_servers': []}})
                    self.assertFalse(Path(env['HTREE_DATA_DIR']).exists())
                    self.assertEqual(env['PATH'].split(os.pathsep)[0], str((ROOT / 'helpers').resolve()))
                    self.assertEqual(env['HTREE_PREFER_LOCAL_DAEMON'], 'false')
                    self.assertEqual(env['HTREE_LOCAL_DAEMON_ONLY'], 'false')
                    self.assertNotIn('GIT_ALTERNATE_OBJECT_DIRECTORIES', env)
                    self.assertIn('--no-local', args[0])
                    self.assertIn('--no-checkout', args[0])
                    self.assertEqual(options['timeout'], 1800)

                with patch.dict(os.environ, HTREE_CONFIG_DIR=publisher, HTREE_DATA_DIR='/cached',
                                GIT_ALTERNATE_OBJECT_DIRECTORIES='/objects'), \
                        patch.object(module.subprocess, 'run', side_effect=inspect_clone), \
                        patch.object(module.subprocess, 'check_output',
                                     side_effect=[revision, 'false', 'objects']) as read:
                    module.verify('htree://npub1fixture/source', revision, ROOT / 'helpers',
                                  clone_timeout=1800, read_servers=selected)
                self.assertEqual(original.read_bytes(), before)
                self.assertEqual(read.call_count, 3)
                self.assertEqual(read.call_args.args[0][-5:],
                                 ['rev-list', '--objects', '--missing=error', revision, '--'])

    def test_unapproved_servers_stop_before_fetch(self):
        for server in ('https://blossom.primal.net', 'https://other.example', 'http://cdn.iris.to',
                       'http://127.0.0.1:8080', 'https://cdn.iris.to/', 'https://cdn.iris.to/path',
                       'https://user@cdn.iris.to', 'https://cdn.iris.to?query', '',
                       'https://cdn.iris.to\nwrite_servers = []'):
            with self.subTest(server=server), patch.object(module.subprocess, 'run') as clone:
                with self.assertRaises(ValueError):
                    module.verify('htree://npub1fixture/source', 'a' * 40, ROOT,
                                  read_servers=['https://cdn.iris.to', server])
                clone.assert_not_called()

    def test_cli_accepts_repeated_servers_and_rejects_unapproved_server(self):
        revision = 'a' * 40
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / 'release.json'
            config.write_text('{"source":"htree://npub1fixture/source"}')
            argv = [str(ROOT / 'scripts/verify-public-source.py'), '--config', str(config),
                    '--commit', revision, '--helpers', str(ROOT), '--clone-timeout', '1800',
                    '--read-server', 'https://cdn.iris.to', '--read-server', 'https://upload.iris.to']

            def inspect_clone(*args, **options):
                settings = (Path(options['env']['HTREE_CONFIG_DIR']) / 'config.toml').read_text()
                self.assertIn('https://cdn.iris.to', settings)
                self.assertIn('https://upload.iris.to', settings)

            with patch.object(sys, 'argv', argv), \
                    patch.object(module.subprocess, 'run', side_effect=inspect_clone) as clone, \
                    patch.object(module.subprocess, 'check_output', side_effect=[revision, 'false', 'objects']):
                runpy.run_path(argv[0], run_name='__main__')
            self.assertEqual(clone.call_args.kwargs['timeout'], 1800)
            with patch.object(sys, 'argv', argv[:-1] + ['https://other.example']), \
                    patch.object(module.subprocess, 'run') as clone, \
                    patch('sys.stderr'):
                with self.assertRaises(SystemExit) as error:
                    runpy.run_path(argv[0], run_name='__main__')
                self.assertEqual(error.exception.code, 2)
                clone.assert_not_called()

    def test_explicit_servers_keep_clone_and_history_failures_fatal(self):
        revision = 'a' * 40
        selected = ['https://cdn.iris.to', 'https://upload.iris.to']
        with patch.object(module.subprocess, 'run', side_effect=subprocess.TimeoutExpired('git', 1800)), \
                patch.object(module.subprocess, 'check_output') as read:
            with self.assertRaises(subprocess.TimeoutExpired):
                module.verify('htree://npub1fixture/source', revision, ROOT,
                              clone_timeout=1800, read_servers=selected)
            read.assert_not_called()
        for outputs, error in ((['b' * 40], ValueError), ([revision, 'true'], ValueError),
                               ([revision, 'false', subprocess.CalledProcessError(128, 'git rev-list')],
                                subprocess.CalledProcessError)):
            with self.subTest(outputs=outputs), patch.object(module.subprocess, 'run'), \
                    patch.object(module.subprocess, 'check_output', side_effect=outputs):
                with self.assertRaises(error):
                    module.verify('htree://npub1fixture/source', revision, ROOT, read_servers=selected)

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
