"""Real CLI audit gate, agent review, rebuild, and immutable payload comparison."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
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
        self.run_cli('identity', 'init')
        self.run_cli('follow', self.author)

    def run_cli(self, *args, home=None, success=True):
        out = subprocess.run([str(BINARY), '--home', str(home or self.home), *args],
                             env=self.env, capture_output=True, text=True, timeout=60)
        self.assertEqual(out.returncode == 0, success, (args, out.stdout, out.stderr))
        return out

    def publish(self, matches=True, version='1.0.0', runnable=False):
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
        (self.source / 'haps-build.toml').write_text(recipe)
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
        self.run_cli('install', 'hello', '--require-attestations', '2', success=False)
        out = self.run_cli('install', 'hello', '--require-attestations', '2', '--allow-unaudited', '--json')
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
        self.run_cli('install', 'hello', '--version', '1.0.0', success=False)
        self.assertFalse((self.home / 'audits').exists())

    def test_audit_bypass_keeps_warnings_and_explicit_review_separate(self):
        self.publish()
        self.run_cli('install', 'hello', '--audit', '--allow-unaudited', success=False)
        self.run_cli('install', 'hello', '--audit-agent', 'codex', '--allow-unaudited', success=False)
        self.run_cli('warn', self.release['id'], '--note', 'Fixture warning')
        self.run_cli('install', 'hello', '--allow-unaudited', success=False)
        self.run_cli('install', 'hello', '--allow-unaudited', '--allow-warnings')

    def test_one_local_review_only_fills_one_missing_audit(self):
        self.publish()
        denied = self.run_cli('install', 'hello', '--require-attestations', '2', '--audit-agent', 'codex', success=False)
        self.assertIn('more independent audits', denied.stderr)
        self.assertFalse((self.root / 'agent-called').exists())
        reviewer = self.root / 'independent-reviewer'
        key = self.run_cli('identity', 'init', home=reviewer).stdout.strip()
        self.run_cli('follow', key)
        claim = self.root / 'independent-audit.json'
        self.run_cli('attest', self.release['id'], '--audited', '--provenance', 'Reviewed fixture payload',
                     '--note', 'Independent fixture review', '--out', str(claim), home=reviewer)
        self.run_cli('import', str(claim))
        self.run_cli('install', 'hello', '--require-attestations', '2', '--audit-agent', 'codex')
        self.assertTrue((self.root / 'agent-called').exists())
        info = json.loads(self.run_cli('info', 'hello', '--json').stdout)
        self.assertEqual(len(info['audits']), 2)

    def test_default_gate_and_explicit_audit_with_unverified_provenance(self):
        self.publish()
        denied = self.run_cli('install', 'hello', '--allow-untrusted', '--allow-warnings', '--require-attestations', '0', '--json', success=False)
        self.assertIn('audit(s)', denied.stdout)
        self.assertFalse((self.root / 'agent-called').exists())
        self.run_cli('attest', self.release['id'], '--note', 'Works for me')
        self.run_cli('install', 'hello', success=False)
        self.run_cli('attest', self.release['id'], '--audited', '--provenance', 'Inspected payload; rebuild not checked', '--note', 'Reviewed fixture')
        self.run_cli('install', 'hello')
        info = json.loads(self.run_cli('info', 'hello', '--json').stdout)
        self.assertFalse(info['audits'][0]['evidence']['binary_match'])
        self.assertFalse((self.root / 'build-called').exists())

    def test_audit_and_install_requires_a_matching_rebuild(self):
        self.publish()
        result = self.run_cli('install', 'hello', '--audit', '--audit-agent', 'codex', '--json')
        self.assertEqual(json.loads(result.stdout)['status'], 'installed')
        self.assertTrue((self.root / 'build-called').exists())
        info = json.loads(self.run_cli('info', 'hello', '--json').stdout)
        self.assertTrue(info['audits'][0]['evidence']['binary_match'])
        self.assertFalse((self.home / 'discovery/outbox').exists())
        self.assertEqual(list((self.home / 'audits/work').iterdir()), [])

    def test_mismatching_rebuild_never_approves_or_installs(self):
        self.publish(matches=False)
        out = self.run_cli('install', 'hello', '--audit', '--audit-agent', 'codex', success=False)
        self.assertIn('does not match', out.stderr)
        self.assertFalse((self.home / 'installed.json').exists())
        info = json.loads(self.run_cli('info', 'hello', '--json').stdout)
        self.assertEqual(info['audits'], [])

    def test_incomplete_review_never_executes_build(self):
        self.publish()
        self.env['AUDIT_VERDICT'] = 'incomplete'
        self.run_cli('install', 'hello', '--audit', '--audit-agent', 'codex', success=False)
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
        self.run_cli('install', 'hello', '--audit-agent', 'codex', success=False)
        self.assertFalse((self.root / 'build-called').exists())
        self.assertEqual(json.loads(self.run_cli('info', 'hello', '--json').stdout)['audits'], [])

    def test_saved_agent_model_and_publish_defaults(self):
        self.publish()
        self.run_cli('audit', 'settings', '--agent', 'codex', '--model', 'fixture-model', '--publish', 'true')
        self.run_cli('install', 'hello', '--audit')
        evidence = json.loads(self.run_cli('info', 'hello', '--json').stdout)['audits'][0]['evidence']
        self.assertEqual(evidence['reviewer'], {'agent': 'codex', 'requested_model': 'fixture-model', 'reported_model': 'fixture-model'})
        self.assertTrue(any((self.home / 'discovery/outbox').iterdir()))

    def test_publication_can_be_overridden_per_audit(self):
        self.publish()
        self.run_cli('audit', 'settings', '--publish', 'true')
        self.run_cli('install', 'hello', '--audit-agent', 'codex', '--publish-audit', 'false')
        self.assertFalse((self.home / 'discovery/outbox').exists())
        self.assertTrue(json.loads(self.run_cli('audit', 'settings').stdout)['publish'])

    def test_external_review_rebuilds_before_recording_approval(self):
        self.publish()
        out = self.run_cli('audit', 'prepare', 'hello')
        session = next((self.home / 'audits/pending').iterdir())
        self.assertIn(str(session / 'repo'), out.stdout)
        self.assertFalse((self.root / 'build-called').exists())
        self.run_cli('audit', 'finish', session.name, '--note', 'Reviewed fixture source and recipe', '--reviewer', 'external-agent', '--model', 'user-claimed-model', '--publish', 'false')
        self.run_cli('install', 'hello')
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
        self.run_cli('install', 'hello', success=False)

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
        term = Terminal(Adapter(), 'install', 'hello', env_extra={})
        self.addCleanup(term.close)
        term.read_until(b'Audit and install')
        term.send(b'y')
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
