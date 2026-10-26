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
if os.environ.get('AGENT_PROMPT'):
    pathlib.Path(os.environ['AGENT_PROMPT']).write_text(prompt)
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
        # These workflow fixtures deliberately write external markers. Their
        # host permission is explicit; build_isolation_test covers the default.
        self.allow_host_build = True
        self.author = self.run_cli('identity', 'init', home=self.publisher).stdout.strip()
        self.package = f'{self.author}/hello'
        self.run_cli('identity', 'init')
        self.run_cli('follow', self.author)

    def run_cli(self, *args, home=None, success=True):
        if self.allow_host_build and (args[0] in ('install', 'update', 'build') or args[:2] == ('audit', 'finish')):
            args = (*args, '--allow-host-build')
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
                 '--audit-agent', 'codex', '--json', '--allow-host-build'], env=self.env,
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

    def test_mismatching_rebuild_installs_only_the_audited_local_output(self):
        self.publish(matches=False)
        out = self.run_cli('install', self.package, '--audit', '--audit-agent', 'codex')
        self.assertIn('installing the local source build', out.stderr)
        receipt = next(iter(json.loads((self.home / 'installed.json').read_text()).values()))
        self.assertNotEqual(receipt['current']['pubkey'], self.author)
        info = json.loads(self.run_cli('info', self.package, '--json').stdout)
        self.assertEqual(info['audits'], [])
        self.assertEqual(len(info['source_reviews']), 1)

    def test_incomplete_review_never_executes_build(self):
        self.publish()
        self.env['AUDIT_VERDICT'] = 'incomplete'
        result = self.run_cli('install', self.package, '--audit', '--audit-agent', 'codex', success=False)
        self.assertFalse((self.root / 'build-called').exists())
        self.assertFalse((self.home / 'installed.json').exists())
        reports = list(self.home.glob('audits/work/*/audit-failure.json'))
        self.assertEqual(len(reports), 1)
        self.assertTrue((reports[0].parent / 'repo/build.py').is_file())
        self.assertIn('audit source retained', result.stderr)
        self.assertFalse((self.home / 'discovery/outbox').exists())

    def repair_fixture(self, runnable=False):
        self.publish(runnable=runnable)
        self.env['AUDIT_VERDICT'] = 'block'
        self.run_cli('install', self.package, '--audit-agent', 'codex', success=False)
        failed = next(self.home.glob('audits/work/*/audit-failure.json'))
        # Preparation must never copy a compromised retained worktree.
        (failed.parent / 'repo/untracked-danger').write_text('do not copy')
        prepared = self.run_cli('audit', 'repair', 'prepare', str(failed))
        session = next((self.home / 'audits/repairs').iterdir())
        self.assertIn('Stop here if the package is not worth repairing', prepared.stdout)
        self.assertFalse((session / 'repo/untracked-danger').exists())
        self.assertEqual(json.loads(failed.read_text()), json.loads((session / 'audit-failure.json').read_text()))
        self.assertFalse((self.root / 'build-called').exists())
        self.assertFalse((self.home / 'installed.json').exists())
        return session

    def commit_repair(self, session, version='1.0.1'):
        source = session / 'repo'
        recipe = source / 'haps-build.toml'
        recipe.write_text(recipe.read_text().replace('version="1.0.0"', f'version="{version}"'))
        def git(*args):
            return subprocess.check_output(['git', '-C', str(source), *args], stderr=subprocess.DEVNULL, text=True).strip()
        git('config', 'user.name', 'Test')
        git('config', 'user.email', 'test@example.invalid')
        git('add', '.')
        git('commit', '--allow-empty', '-m', 'Repair fixture')
        return git('rev-parse', 'HEAD')

    def build_repair(self, session, revision, success=True, repository=None):
        return self.run_cli('audit', 'repair', 'build', session.name,
                            '--repository', repository or (session / 'repo').as_uri(),
                            '--rev', revision, '--note', 'Fixed the fixture finding',
                            '--audit-agent', 'codex', '--allow-host-build', success=success)

    def test_repair_can_be_declined_and_rejected_again_without_build_or_publication(self):
        session = self.repair_fixture()
        revision = self.commit_repair(session)
        for verdict in ('block', 'incomplete'):
            with self.subTest(verdict=verdict):
                self.env['AUDIT_VERDICT'] = verdict
                self.build_repair(session, revision, success=False)
                self.assertFalse((self.root / 'build-called').exists())
                self.assertIsNone(json.loads((session / 'ready.json').read_text()))
                self.run_cli('audit', 'repair', 'publish', session.name, '--name', 'fixed', success=False)
        self.assertFalse((self.home / 'discovery/outbox').exists())
        self.assertFalse((self.home / 'installed.json').exists())

    def test_repair_reviews_fresh_commit_and_signs_only_own_payload(self):
        session = self.repair_fixture()
        revision = self.commit_repair(session)
        self.env['AUDIT_VERDICT'] = 'pass'
        self.env['AGENT_PROMPT'] = str(self.root / 'prompt')
        self.run_cli('audit', 'settings', '--publish', 'true')
        # Dirty edits are not part of the explicitly pinned commit.
        (session / 'repo/build.py').write_text('raise RuntimeError("dirty checkout executed")')
        built = self.build_repair(session, revision)
        self.assertIn('Nothing installed or published', built.stdout)
        self.assertTrue((self.root / 'build-called').exists())
        prompt = (self.root / 'prompt').read_text()
        self.assertIn('repair_review', prompt)
        self.assertIn(self.release['id'], prompt)
        self.assertIn('Fixed the fixture finding', prompt)
        self.assertNotIn('dirty checkout executed', prompt)
        self.assertFalse((self.home / 'discovery/outbox').exists())
        self.assertFalse((self.home / 'installed.json').exists())
        own = self.run_cli('identity', 'show').stdout.strip()
        info = json.loads(self.run_cli('info', f'{own}/hello', '--json').stdout)
        evidence = info['audits'][0]['evidence']
        self.assertIn(self.release['id'], evidence['provenance'])
        self.assertIn('Fixed the fixture finding', evidence['provenance'])
        self.assertFalse(evidence['binary_match'])
        self.assertEqual(json.loads(self.run_cli('info', self.package, '--json').stdout)['audits'], [])
        denied = self.run_cli('audit', 'repair', 'publish', session.name, '--name', 'fixed', success=False)
        self.assertIn('local source paths cannot be published', denied.stderr)
        self.run_cli('install', f'{own}/hello')
        receipt = next(iter(json.loads((self.home / 'installed.json').read_text()).values()))
        self.assertEqual(receipt['current']['pubkey'], own)
        catalog = session / 'builds' / json.loads((session / 'ready.json').read_text())['attempt'] / 'catalog'
        self.assertFalse((catalog / 'audit-failure.json').exists())
        # Another reader must be able to audit/reproduce the committed version
        # despite the content-derived +haps suffix on the signed release.
        reader = self.root / 'independent-reader'
        self.run_cli('identity', 'init', home=reader)
        self.run_cli('follow', own, home=reader)
        self.run_cli('source', 'add', 'repair', str(catalog), '--author', own, home=reader)
        self.run_cli('install', f'{own}/hello', '--audit-agent', 'codex', home=reader)
        reviewed = json.loads(self.run_cli('info', f'{own}/hello', '--json', home=reader).stdout)
        self.assertTrue(reviewed['audits'][0]['evidence']['binary_match'])
        installed = next(iter(json.loads((reader / 'installed.json').read_text()).values()))
        self.assertEqual(installed['current']['pubkey'], own)

    def test_failed_repair_retry_invalidates_publish_selection_but_preserves_previous_build(self):
        session = self.repair_fixture()
        revision = self.commit_repair(session)
        self.env['AUDIT_VERDICT'] = 'pass'
        self.build_repair(session, revision)
        previous = json.loads((session / 'ready.json').read_text())
        self.env['AUDIT_VERDICT'] = 'block'
        self.build_repair(session, revision, success=False)
        self.assertTrue((session / 'builds' / previous['attempt'] / 'catalog/catalog.json').exists())
        result = self.run_cli('audit', 'repair', 'publish', session.name, '--name', 'fixed', success=False)
        self.assertIn('latest repair attempt did not complete', result.stderr)

    @unittest.skipUnless(sys.platform in ('linux', 'darwin'), 'requires native build isolation')
    def test_repair_defaults_to_isolated_build_with_network_disabled(self):
        session = self.repair_fixture(runnable=True)
        revision = self.commit_repair(session)
        self.env['AUDIT_VERDICT'] = 'pass'
        out = self.run_cli('audit', 'repair', 'build', session.name,
                           '--repository', (session / 'repo').as_uri(), '--rev', revision,
                           '--note', 'Reviewed fixture fix', '--audit-agent', 'codex')
        self.assertIn('Isolated files and environment; network disabled.', out.stderr)
        self.assertIsNotNone(json.loads((session / 'ready.json').read_text()))

    def test_repair_requires_new_version_and_real_descendant_commit(self):
        session = self.repair_fixture()
        original = json.loads((session / 'audit-failure.json').read_text())['recipe']['package']['source']['rev']
        denied = self.build_repair(session, original, success=False)
        self.assertIn('newer package version', denied.stderr)
        revision = self.commit_repair(session)
        # Re-root the same tree: this is an unrelated project, not a descendant repair.
        source = session / 'repo'
        tree = subprocess.check_output(['git', '-C', str(source), 'rev-parse', f'{revision}^{{tree}}'], text=True).strip()
        unrelated = subprocess.check_output(['git', '-C', str(source), 'commit-tree', tree, '-m', 'Unrelated'], text=True).strip()
        subprocess.run(['git', '-C', str(source), 'update-ref', 'refs/heads/unrelated', unrelated], check=True)
        denied = self.build_repair(session, unrelated, success=False)
        self.assertIn('audited commit in its history', denied.stderr)
        self.assertFalse((self.root / 'build-called').exists())
        self.run_cli('audit', 'repair', 'build', '../escape', '--repository', source.as_uri(), '--rev', revision,
                     '--note', 'Fix', '--audit-agent', 'codex', success=False)

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

    def terminal(self, *args):
        from chooser_test import Terminal
        fixture = self
        class Adapter:
            def args(self, _, *arguments):
                return [str(BINARY), '--home', str(fixture.home), *arguments, '--allow-host-build']
            def env(self, _):
                return {**fixture.env, 'HAPS_NON_INTERACTIVE': 'false', 'TERM': 'xterm-256color'}
        term = Terminal(Adapter(), *args)
        self.addCleanup(term.close)
        return term

    def test_noninteractive_block_never_uses_successful_audit_publication_preference(self):
        self.publish()
        self.run_cli('audit', 'settings', '--publish', 'true')
        self.env['AUDIT_VERDICT'] = 'block'
        self.run_cli('install', self.package, '--audit-agent', 'codex', success=False)
        self.assertFalse((self.home / 'discovery/outbox').exists())
        self.assertFalse((self.root / 'build-called').exists())
        self.assertEqual(len(list(self.home.glob('audits/work/*/audit-failure.json'))), 1)

    def blocked_warning(self, answer):
        self.publish()
        self.run_cli('audit', 'settings', '--publish', 'true')
        self.env['AUDIT_VERDICT'] = 'block'
        term = self.terminal('install', self.package, '--audit-agent', 'codex')
        term.read_until(b'Publish this warning?', timeout=30)
        self.assertIn(b'Public warning preview for hello 1.0.0', term.output)
        self.assertIn(b'Source review blocked this release: Reviewed the fixture source and build recipe.', term.output)
        self.assertFalse((self.home / 'discovery/outbox').exists())
        term.send(answer)
        code, output = term.finish()
        self.assertNotEqual(code, 0, output)
        self.assertFalse((self.root / 'build-called').exists())
        self.assertFalse((self.home / 'installed.json').exists())
        self.assertEqual(len(list(self.home.glob('audits/work/*/audit-failure.json'))), 1)
        return json.loads(self.run_cli('info', self.package, '--json').stdout)

    @unittest.skipUnless(os.name == 'posix', 'requires a terminal')
    def test_public_warning_defaults_to_no_even_when_passing_audits_are_published(self):
        info = self.blocked_warning(b'\n')
        self.assertEqual(info['warnings'], [])
        self.assertFalse((self.home / 'discovery/outbox').exists())

    @unittest.skipUnless(os.name == 'posix', 'requires a terminal')
    def test_public_warning_is_signed_only_after_explicit_yes(self):
        info = self.blocked_warning(b'y')
        self.assertEqual(len(info['warnings']), 1)
        event = info['warnings'][0]['event']
        self.assertIn(self.release['id'], json.dumps(event))
        self.assertTrue(any((self.home / 'discovery/outbox').iterdir()))

    @unittest.skipUnless(os.name == 'posix', 'requires a terminal')
    def test_terminal_review_progress_updates_in_place(self):
        self.publish()
        gate = self.root / 'continue-review'
        agent = self.bin / 'codex'
        agent.write_text(agent.read_text().replace('out.write_text(',
            f'import time\nwhile not pathlib.Path({str(gate)!r}).exists(): time.sleep(0.02)\nout.write_text('))
        term = self.terminal('install', self.package, '--audit-agent', 'codex', '--publish-audit', 'false')
        term.read_until(b'Reviewing source with codex', timeout=30)
        start = term.output.index(b'Reviewing source with codex')
        try:
            term.read_until(b'5s elapsed', timeout=10)
            interval = term.output[start:]
            self.assertIn(b'\r\x1b[2K', interval)
            self.assertNotIn(b'\n', interval)
        finally:
            gate.touch()
        code, output = term.finish(timeout=30)
        self.assertEqual(code, 0, output)

    @unittest.skipUnless(os.name == 'posix', 'requires a terminal')
    def test_terminal_first_audit_offers_review_and_remembers_publication_choice(self):
        from chooser_test import Terminal
        self.publish()
        fixture = self
        class Adapter:
            def args(self, _, *args):
                return [str(BINARY), '--home', str(fixture.home), *args, '--allow-host-build']
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
        term.send(b'y')
        term.read_until(b'Choose how to audit')
        term.send(b'\n')
        term.read_until(b'Publish this and future audits', timeout=30)
        term.send(b'n')
        code, output = term.finish()
        self.assertEqual(code, 0, output)
        self.assertNotIn('Build reviewed source', output)
        self.assertNotIn('Verify the published binary', output)
        self.assertFalse(json.loads(self.run_cli('audit', 'settings').stdout)['publish'])
        self.assertIn('Rebuilt payload matches', output)


if __name__ == '__main__':
    unittest.main()
