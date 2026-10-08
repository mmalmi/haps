import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('security', ROOT / 'src/integrations/security.py')
security = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(security)
BINARY = Path(os.environ.get('HAPS_TEST_BINARY', ROOT / 'target/debug/haps'))


@unittest.skipUnless(os.name == 'posix', 'executable scanner fixture')
class SourceSecurity(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="haps scan ' $ ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.home = self.root / 'home'
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        self.rules = self.root / 'rules.yaml'
        self.rules.write_text('rules: []\n')
        self.scanner = self.root / 'semgrep'
        self.scanner.write_text(f'''#!{sys.executable}
import json, os, pathlib, sys
if sys.argv[1:] == ['--version']:
    print('1.0.0'); sys.exit()
assert 'SEMGREP_APP_TOKEN' not in os.environ
assert '--disable-nosem' in sys.argv and '--no-git-ignore' in sys.argv
assert '--metrics=off' in sys.argv and '--oss-only' in sys.argv
source = pathlib.Path(sys.argv[-1])
assert (source / '.semgrepignore').read_text() == ''
mode = (source / 'mode').read_text()
result = {{'version': '1.0.0', 'results': [], 'errors': [], 'paths': {{'scanned': [str(source / 'build.py')]}}}}
if mode == 'finding': result['results'] = [{{'check_id': 'danger'}}]
if mode == 'error': result['errors'] = [{{'message': 'parse error'}}]
if mode == 'empty': result['paths']['scanned'] = []
if mode == 'wrongpath': result['paths']['scanned'] = ['/etc/passwd']
print(json.dumps(result))
''')
        self.scanner.chmod(0o755)
        self.git('init')
        self.git('config', 'user.name', 'Test')
        self.git('config', 'user.email', 'test@example.invalid')
        (self.repo / 'build.py').write_text('from pathlib import Path\nPath("hello").write_text("Built after scan")\n')
        (self.repo / '.semgrepignore').write_text('*\n')
        self.revision = self.commit('clean')
        self.capture(security.configure, self.home, [str(self.rules), str(self.scanner)])

    def git(self, *args):
        return subprocess.check_output(['git', '-C', str(self.repo), *args], text=True, stderr=subprocess.DEVNULL).strip()

    def commit(self, mode):
        (self.repo / 'mode').write_text(mode)
        self.git('add', '.')
        self.git('commit', '-qm', mode)
        return self.git('rev-parse', 'HEAD')

    def capture(self, function, *args):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            function(*args)
        return output.getvalue()

    def test_private_pinned_scan_and_failures(self):
        report = json.loads(self.capture(security.scan, self.home, self.repo, self.revision))
        self.assertEqual(report['source_commit'], self.revision)
        self.assertFalse(report['binary_source_verified'])
        self.assertEqual(report['rules_sha256'], hashlib.sha256(self.rules.read_bytes()).hexdigest())
        self.assertNotIn(str(self.root), json.dumps(report))
        self.assertEqual((self.home / 'security').stat().st_mode & 0o777, 0o700)
        for path in (self.home / 'security/reports').iterdir():
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        for mode in ('finding', 'error', 'empty', 'wrongpath'):
            with self.subTest(mode=mode):
                revision = self.commit(mode)
                with self.assertRaises(ValueError):
                    security.scan(self.home, self.repo, revision)
        rules = next((self.home / 'security/rules').iterdir())
        rules.write_text('changed')
        with self.assertRaisesRegex(ValueError, 'rules changed'):
            security.scan(self.home, self.repo, revision)

    def test_symlink_is_incomplete_not_a_pass(self):
        (self.repo / 'outside').symlink_to('/etc/passwd')
        revision = self.commit('link')
        with self.assertRaisesRegex(ValueError, 'symlinks or submodules'):
            security.scan(self.home, self.repo, revision)

    def test_missing_scanner_timeout_and_output_limits(self):
        with self.assertRaises(ValueError):
            security.configure(self.root / 'missing-home', [str(self.rules), 'missing-haps-scanner'])
        directory = self.root / 'process'
        directory.mkdir()
        with self.assertRaisesRegex(ValueError, 'timed out'):
            security.run_scanner([sys.executable, '-c', 'import time; time.sleep(60)'], directory, 0.1)
        with patch.object(security, 'MAX_OUTPUT', 64):
            with self.assertRaisesRegex(ValueError, 'output exceeded'):
                security.run_scanner([sys.executable, '-c', 'print("x"*100)'], directory, 30)

    def test_source_build_gate_and_explicit_signed_scan(self):
        self.assertTrue(BINARY.exists(), 'build haps before integration tests')
        recipe = f'''[package]
name = "hello"
version = "1.0.0"
target = "host"
description = "Source scan fixture"
[build]
commands = [[{json.dumps(sys.executable)}, "build.py"]]
[build.artifacts]
"hello" = "hello"
'''
        (self.repo / 'haps-build.toml').write_text(recipe)
        agent = self.root / 'codex'
        agent.write_text('#!/usr/bin/env python3\nimport json, pathlib, sys\nsys.stdin.read()\npathlib.Path(sys.argv[sys.argv.index("--output-last-message") + 1]).write_text(json.dumps({"verdict":"pass", "note":"Reviewed source fixture"}))\n')
        agent.chmod(0o755)
        env = dict(os.environ, HAPS_NO_DEFAULTS='true', HAPS_PYTHON=sys.executable,
                   HTREE_CONFIG_DIR=str(self.root / 'hashtree'), PATH=str(self.root) + os.pathsep + os.environ['PATH'])

        def run(*args, success=True):
            result = subprocess.run([str(BINARY), '--home', str(self.home), *args],
                                    env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode == 0, success, result.stderr + result.stdout)
            return result

        run('identity', 'init')
        bad = self.commit('finding')
        run('build', self.repo.as_uri(), '--rev', bad)  # Preview does not scan or execute.
        self.assertFalse((self.home / 'built-packages').exists())
        failed = run('build', self.repo.as_uri(), '--rev', bad, '--execute', '--install', '--audit-agent', 'codex', success=False)
        self.assertIn('finding', failed.stderr)
        self.assertFalse((self.home / 'built-packages').exists())
        clean = self.commit('clean')
        run('build', self.repo.as_uri(), '--rev', clean, '--execute', '--install', '--audit-agent', 'codex')
        self.assertFalse((self.home / 'discovery/outbox').exists(), 'scans must not publish by default')
        (self.repo / 'haps-build.toml').write_text(recipe.replace('1.0.0', '1.1.0'))
        clean = self.commit('clean')
        run('build', self.repo.as_uri(), '--rev', clean, '--execute', '--install', '--audit-agent', 'codex', '--attest-scan')
        event_files = list((self.home / 'discovery/outbox').glob('*.json'))
        self.assertEqual(len(event_files), 1)
        event = json.loads(event_files[0].read_text())
        self.assertEqual(event['kind'], 1111)
        report = json.loads(event['content'])
        self.assertEqual(report['schema'], 'haps.source-scan.v1')
        self.assertEqual(report['source_commit'], clean)
        self.assertEqual(report['result'], 'no_findings')
        self.assertNotIn(str(self.root), event['content'])
        self.assertNotIn('approved', report)
        # Binary installs and updates must not invoke the source scan policy.
        author = run('identity', 'show').stdout.strip()
        run('source', 'add', 'local', str(self.home / 'built-packages'), '--author', author)
        self.scanner.unlink()
        run('install', f'{author}/hello')
        run('update', 'hello')
        refused = run('install', f'{author}/hello', '--require-attestations', '2', success=False)
        self.assertIn('audit(s)', refused.stderr)


if __name__ == '__main__':
    unittest.main()
