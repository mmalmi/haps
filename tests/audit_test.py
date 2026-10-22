"""Real CLI audit gate, agent review, rebuild, and immutable payload comparison."""
import json
import os
import queue
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
import importlib.util

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get('HAPS_TEST_BINARY', ROOT / 'target/debug/haps')).resolve()


class AgentDetection(unittest.TestCase):
    def test_omarchy_preference_resolves_installed_agent_without_running_lazy_installer(self):
        spec = importlib.util.spec_from_file_location('audit_adapter', ROOT / 'src/integrations/audit.py')
        adapter = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(adapter)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            default = root / '.config/omarchy/defaults/agent'
            default.parent.mkdir(parents=True)
            default.write_text('claude\n')
            real = root / 'installed-claude'
            real.write_text('#!/bin/sh\nexit 0\n')
            real.chmod(0o755)
            for name, text in {
                'codex': '#!/bin/sh\nexit 0\n',
                'claude': '#!/bin/sh\nmise use -g claude\nexit 99\n',
                'mise': f'#!/bin/sh\n[ "$1" = which ] || exit 99\nprintf "%s\\n" "{real}"\n',
            }.items():
                (root / name).write_text(text)
                (root / name).chmod(0o755)
            with patch.dict(os.environ, HOME=str(root), PATH=str(root)):
                self.assertEqual(adapter.agents(), ['claude', 'codex', 'manual'])
                self.assertEqual(adapter.executable('claude'), str(real))
                real.unlink()
                self.assertEqual(adapter.agents(), ['codex', 'manual'])


class Audits(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.home = self.root / 'reader'
        self.publisher = self.root / 'publisher'
        self.source = self.root / 'source'
        self.source.mkdir()
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        agent = self.bin / 'codex'
        agent.write_text('#!/usr/bin/env python3\n' + '''import json, os, pathlib, sys
prompt = sys.stdin.read()
assert 'UNTRUSTED SNAPSHOT' in prompt
assert '--ignore-user-config' in sys.argv
assert '--sandbox' in sys.argv and 'read-only' in sys.argv
assert 'shell_tool' in sys.argv
pathlib.Path(os.environ['AGENT_CALLED']).write_text('called')
if '--model' in sys.argv:
    print('model: ' + sys.argv[sys.argv.index('--model') + 1], file=sys.stderr)
out = pathlib.Path(sys.argv[sys.argv.index('--output-last-message') + 1])
out.write_text(json.dumps({'verdict': os.environ.get('AUDIT_VERDICT', 'pass'), 'note': 'Reviewed the fixture source and build recipe.'}))
''')
        agent.chmod(0o755)
        self.env = dict(os.environ, HAPS_NO_DEFAULTS='true', HAPS_NON_INTERACTIVE='true',
                        PATH=str(self.bin) + os.pathsep + os.environ['PATH'],
                        AGENT_CALLED=str(self.root / 'agent-called'), HTREE_CONFIG_DIR=str(self.root / 'htree'))
        self.env.pop('NOSTR_RELAYS', None)
        self.author = self.run_cli('identity', 'init', home=self.publisher).stdout.strip()
        self.package = f'{self.author}/hello'
        self.run_cli('identity', 'init')
        self.run_cli('follow', self.author)

    def run_cli(self, *args, home=None, success=True):
        out = subprocess.run([str(BINARY), '--home', str(home or self.home), *args],
                             env=self.env, capture_output=True, text=True, timeout=60)
        self.assertEqual(out.returncode == 0, success, (args, out.stdout, out.stderr))
        return out

    def publish(self, matches=True, version='1.0.0', runnable=False, recipe_style='legacy'):
        def git(*args):
            return subprocess.check_output(['git', '-C', str(self.source), *args], stderr=subprocess.DEVNULL, text=True).strip()
        git('init')
        git('config', 'user.name', 'Test')
        git('config', 'user.email', 'test@example.invalid')
        target = self.run_cli('target').stdout.strip()
        spec = f'name="hello"\nversion="{version}"\ntarget={json.dumps(target)}\ndescription="Audit fixture"\n[commands]\nhello="hello"\n'
        marker = self.root / 'build-called'
        content = b'#!/usr/bin/env python3\nprint("Haps audit test passed")\n' if runnable else b'expected'
        marker_command = '' if runnable else f'Path({str(marker)!r}).write_text("built")\n'
        (self.source / 'build.py').write_text(f'from pathlib import Path\n{marker_command}Path("hello").write_bytes({content!r})\n')
        recipe = '[package]\n' + spec.replace('[commands]', '[package.commands]')
        recipe += '\n[build]\ncommands = [[' + json.dumps(sys.executable) + ', "build.py"]]\n[build.artifacts]\nhello="hello"\n'
        if recipe_style == 'legacy':
            (self.source / 'haps-build.toml').write_text(recipe)
        elif recipe_style == 'unified':
            (self.source / 'haps.toml').write_text(spec + recipe[recipe.index('\n[build]'):])
        elif recipe_style == 'unavailable':
            (self.source / 'haps.toml').write_text(spec + '\n[build]\nunavailable="Needs platform signing setup"\n')
        elif recipe_style == 'none':
            (self.source / 'haps.toml').write_text(spec)
            (self.source / 'README.md').write_text('Build with python3 build.py. The output is hello.\n')
        else:
            raise ValueError(recipe_style)
        git('add', '.')
        git('commit', '-m', 'fixture')
        revision = git('rev-parse', 'HEAD')
        spec += f'\n[source]\ngit={json.dumps(self.source.as_uri())}\nrev={json.dumps(revision)}\n'
        manifest = self.root / 'haps.toml'
        manifest.write_text(spec)
        payload = self.root / 'payload'
        payload.mkdir(exist_ok=True)
        (payload / 'hello').write_bytes(content if matches else b'publisher mismatch')
        repository = self.root / 'repo'
        self.release = json.loads(self.run_cli('pack', str(manifest), '--payload', str(payload), '--out', str(repository), home=self.publisher).stdout)
        if not getattr(self, 'source_added', False):
            self.run_cli('source', 'add', 'fixture', str(repository), '--author', self.author)
            self.source_added = True

    def test_audit_bypass_is_per_operation_and_preserves_the_threshold(self):
        self.publish()
        self.run_cli('install', self.package, '--require-attestations', '2', success=False)
        out = self.run_cli('install', self.package, '--require-attestations', '2', '--allow-unaudited', '--json')
        self.assertEqual(json.loads(out.stdout)['status'], 'installed')
        self.assertIn('audit requirement', out.stderr)
        self.assertFalse((self.root / 'agent-called').exists())
        self.assertFalse((self.root / 'build-called').exists())
        self.assertEqual(json.loads(self.run_cli('info', 'hello', '--json').stdout)['audits'], [])
        self.publish(version='1.1.0')
        self.run_cli('update', 'hello', success=False)
        self.run_cli('update', 'hello', '--allow-unaudited')
        receipts = json.loads((self.home / 'installed.json').read_text())
        self.assertEqual(next(iter(receipts.values()))['minimum_attestations'], 2)
        self.run_cli('rollback', 'hello', success=False)
        self.run_cli('rollback', 'hello', '--allow-unaudited')
        self.run_cli('install', self.package, '--version', '1.0.0', success=False)
        self.assertFalse((self.home / 'audits').exists())

    def test_audit_bypass_keeps_warnings_and_explicit_review_separate(self):
        self.publish()
        self.run_cli('install', self.package, '--audit', '--allow-unaudited', success=False)
        self.run_cli('install', self.package, '--audit-agent', 'codex', '--allow-unaudited', success=False)
        self.run_cli('warn', self.release['id'], '--note', 'Fixture warning')
        self.run_cli('install', self.package, '--allow-unaudited', success=False)
        self.run_cli('install', self.package, '--allow-unaudited', '--allow-warnings')

    def test_one_local_review_only_fills_one_missing_audit(self):
        self.publish()
        denied = self.run_cli('install', self.package, '--require-attestations', '2', '--audit-agent', 'codex', success=False)
        self.assertIn('more independent audits', denied.stderr)
        self.assertFalse((self.root / 'agent-called').exists())
        reviewer = self.root / 'independent-reviewer'
        key = self.run_cli('identity', 'init', home=reviewer).stdout.strip()
        self.run_cli('follow', key)
        claim = self.root / 'independent-audit.json'
        self.run_cli('attest', self.release['id'], '--audited', '--provenance', 'Reviewed fixture payload',
                     '--note', 'Independent fixture review', '--out', str(claim), home=reviewer)
        self.run_cli('import', str(claim))
        self.run_cli('install', self.package, '--require-attestations', '2', '--audit-agent', 'codex')
        self.assertTrue((self.root / 'agent-called').exists())
        info = json.loads(self.run_cli('info', 'hello', '--json').stdout)
        self.assertEqual(len(info['audits']), 2)

    def test_default_gate_and_explicit_audit_with_unverified_provenance(self):
        self.publish()
        denied = self.run_cli('install', self.package, '--allow-untrusted', '--allow-warnings', '--require-attestations', '0', '--json', success=False)
        self.assertIn('audit(s)', denied.stdout)
        self.assertFalse((self.root / 'agent-called').exists())
        self.run_cli('attest', self.release['id'], '--note', 'Works for me')
        self.run_cli('install', self.package, success=False)
        self.run_cli('attest', self.release['id'], '--audited', '--provenance', 'Inspected payload; rebuild not checked', '--note', 'Reviewed fixture')
        self.run_cli('install', self.package)
        info = json.loads(self.run_cli('info', 'hello', '--json').stdout)
        self.assertFalse(info['audits'][0]['evidence']['binary_match'])
        self.assertFalse((self.root / 'build-called').exists())

    def test_audit_and_install_requires_a_matching_rebuild(self):
        self.publish()
        result = self.run_cli('install', self.package, '--audit', '--audit-agent', 'codex', '--json')
        self.assertEqual(json.loads(result.stdout)['status'], 'installed')
        self.assertTrue((self.root / 'build-called').exists())
        info = json.loads(self.run_cli('info', 'hello', '--json').stdout)
        self.assertTrue(info['audits'][0]['evidence']['binary_match'])
        self.assertFalse((self.home / 'discovery/outbox').exists())
        self.assertEqual(list((self.home / 'audits/work').iterdir()), [])

    def test_audit_reports_source_and_agent_waits_before_completion(self):
        self.publish()
        fetch_gate = self.root / 'fetch-continue'
        review_gate = self.root / 'review-continue'
        real_git = shutil.which('git')
        git = self.bin / 'git'
        git.write_text('#!/usr/bin/env python3\n' +
                       'import os, sys, time\nfrom pathlib import Path\n' +
                       f'gate = Path({str(fetch_gate)!r})\n' +
                       'if "clone" in sys.argv[1:]:\n'
                       '    while not gate.exists(): time.sleep(0.02)\n' +
                       f'os.execv({real_git!r}, [{real_git!r}, *sys.argv[1:]])\n')
        git.chmod(0o755)
        agent = self.bin / 'codex'
        agent.write_text(agent.read_text().replace(
            "out = pathlib.Path", f'import time\nwhile not pathlib.Path({str(review_gate)!r}).exists(): time.sleep(0.02)\nout = pathlib.Path'))
        lines = queue.Queue()
        transcript = []

        def until(label, heartbeat=False):
            deadline = time.monotonic() + 9
            while True:
                try:
                    line = lines.get(timeout=max(0, deadline - time.monotonic()))
                except queue.Empty:
                    self.fail(f'No live {label!r} (heartbeat={heartbeat}): {"".join(transcript)}')
                transcript.append(line)
                if label in line and (not heartbeat or 's elapsed)' in line):
                    return

        with tempfile.TemporaryFile(mode='w+') as stdout:
            process = subprocess.Popen(
                [str(BINARY), '--home', str(self.home), 'install', self.package,
                 '--audit-agent', 'codex', '--json'], env=self.env,
                stdout=stdout, stderr=subprocess.PIPE, text=True)

            def read_stderr():
                for line in process.stderr:
                    lines.put(line)

            reader = threading.Thread(target=read_stderr, daemon=True)
            reader.start()
            try:
                until('Fetching pinned source for audit')
                until('Fetching pinned source for audit', heartbeat=True)
                self.assertFalse((self.root / 'agent-called').exists())
                self.assertFalse((self.home / 'installed.json').exists())
                fetch_gate.touch()
                until('Reviewing source with codex')
                until('Reviewing source with codex', heartbeat=True)
                self.assertFalse((self.root / 'build-called').exists())
                self.assertFalse((self.home / 'installed.json').exists())
                review_gate.touch()
                self.assertEqual(process.wait(timeout=20), 0)
                reader.join(timeout=2)
                stdout.seek(0)
                self.assertEqual(json.loads(stdout.read())['status'], 'installed')
            finally:
                fetch_gate.touch()
                review_gate.touch()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                reader.join(timeout=2)
                process.stderr.close()

    def test_mismatching_rebuild_never_approves_or_installs(self):
        self.publish(matches=False)
        out = self.run_cli('install', self.package, '--audit', '--audit-agent', 'codex', success=False)
        self.assertIn('does not match', out.stderr)
        self.assertFalse((self.home / 'installed.json').exists())
        info = json.loads(self.run_cli('info', 'hello', '--json').stdout)
        self.assertEqual(info['audits'], [])

    def test_incomplete_review_never_executes_build(self):
        self.publish()
        self.env['AUDIT_VERDICT'] = 'incomplete'
        self.run_cli('install', self.package, '--audit', '--audit-agent', 'codex', success=False)
        self.assertFalse((self.root / 'build-called').exists())
        self.assertFalse((self.home / 'installed.json').exists())

    def test_enabled_source_scanner_still_blocks_an_audit_build(self):
        self.publish()
        scanner = self.bin / 'semgrep'
        scanner.write_text('#!/usr/bin/env python3\nimport sys\nif "--version" in sys.argv: print("1.0.0")\nelse: sys.exit(2)\n')
        scanner.chmod(0o755)
        rules = self.root / 'rules.yaml'
        rules.write_text('rules: []\n')
        self.run_cli('security', '--rules', str(rules), '--scanner', str(scanner))
        self.run_cli('install', self.package, '--audit-agent', 'codex', success=False)
        self.assertFalse((self.root / 'build-called').exists())
        self.assertEqual(json.loads(self.run_cli('info', 'hello', '--json').stdout)['audits'], [])

    def test_saved_agent_model_and_publish_defaults(self):
        self.publish()
        self.run_cli('audit', 'settings', '--agent', 'codex', '--model', 'fixture-model', '--publish', 'true')
        self.run_cli('install', self.package, '--audit')
        evidence = json.loads(self.run_cli('info', 'hello', '--json').stdout)['audits'][0]['evidence']
        self.assertEqual(evidence['reviewer'], {'agent': 'codex', 'requested_model': 'fixture-model', 'reported_model': 'fixture-model'})
        self.assertTrue(any((self.home / 'discovery/outbox').iterdir()))

    def test_publication_can_be_overridden_per_audit(self):
        self.publish()
        self.run_cli('audit', 'settings', '--publish', 'true')
        self.run_cli('install', self.package, '--audit-agent', 'codex', '--publish-audit', 'false')
        self.assertFalse((self.home / 'discovery/outbox').exists())
        self.assertTrue(json.loads(self.run_cli('audit', 'settings').stdout)['publish'])

    def test_external_review_rebuilds_before_recording_approval(self):
        self.publish()
        out = self.run_cli('audit', 'prepare', 'hello')
        session = next((self.home / 'audits/pending').iterdir())
        self.assertIn(str(session / 'repo'), out.stdout)
        self.assertFalse((self.root / 'build-called').exists())
        self.run_cli('audit', 'finish', session.name, '--note', 'Reviewed fixture source and recipe', '--reviewer', 'external-agent', '--model', 'user-claimed-model', '--publish', 'false')
        self.run_cli('install', self.package)
        evidence = json.loads(self.run_cli('info', 'hello', '--json').stdout)['audits'][0]['evidence']
        self.assertTrue(evidence['binary_match'])
        self.assertNotIn('reported_model', evidence['reviewer'])

    def test_modified_prepared_source_never_builds(self):
        self.publish()
        self.run_cli('audit', 'prepare', 'hello')
        session = next((self.home / 'audits/pending').iterdir())
        (session / 'repo/build.py').write_text('raise RuntimeError("changed")')
        result = self.run_cli('audit', 'finish', session.name, '--note', 'Reviewed', success=False)
        self.assertIn('prepared source has changed', result.stderr)
        self.assertFalse((self.root / 'build-called').exists())
        self.run_cli('install', self.package, success=False)

    @unittest.skipUnless(os.name == 'posix', 'requires a terminal')
    def test_terminal_first_audit_offers_review_and_remembers_publication_choice(self):
        from chooser_test import Terminal
        self.publish()
        fixture = self
        class Adapter:
            def args(self, _, *args):
                return [str(BINARY), '--home', str(fixture.home), *args]
            def env(self, _):
                return {**fixture.env, 'HAPS_NON_INTERACTIVE': 'false', 'TERM': 'xterm-256color'}
        declined = Terminal(Adapter(), 'install', 'hello', '--audit-agent', 'codex')
        self.addCleanup(declined.close)
        declined.read_until(b'Install this package?')
        declined.send(b'n')
        code, output = declined.finish()
        self.assertNotEqual(code, 0, output)
        self.assertIn('cancelled', output)
        self.assertFalse((self.root / 'agent-called').exists())
        self.assertFalse((self.root / 'build-called').exists())
        self.assertFalse((self.home / 'installed.json').exists())
        term = Terminal(Adapter(), 'install', 'hello', env_extra={})
        self.addCleanup(term.close)
        term.read_until(b'Install this package?')
        self.assertFalse((self.root / 'agent-called').exists())
        self.assertFalse((self.root / 'build-called').exists())
        term.send(b'y')
        term.read_until(b'Audit and install')
        term.send(b'\x1b[B\n')
        term.read_until(b'Choose how to audit')
        term.send(b'\n')
        term.read_until(b'Publish this and future audits', timeout=30)
        term.send(b'n')
        code, output = term.finish()
        self.assertEqual(code, 0, output)
        self.assertFalse(json.loads(self.run_cli('audit', 'settings').stdout)['publish'])
        self.assertIn('Rebuilt payload matches', output)


if __name__ == '__main__':
    unittest.main()
