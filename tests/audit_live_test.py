"""Opt-in real-agent terminal qualification; uses the agent's existing sign-in.

HAPS_LIVE_AUDIT=1 HAPS_TEST_BINARY=/path/to/haps python3 -m unittest discover \
    -s tests -p audit_live_test.py
"""
import importlib.util
import json
import os
import subprocess
from pathlib import Path
import unittest

import audit_test
from chooser_test import Terminal


@unittest.skipUnless(os.name == 'posix' and os.environ.get('HAPS_LIVE_AUDIT') == '1',
                     'requires explicit opt-in, a terminal, and an installed signed-in agent')
class LiveAudit(unittest.TestCase):
    def test_real_agent_plans_from_project_instructions_then_reviews_local_build(self):
        fixture = audit_test.Audits('runTest')
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        fixture.env['PATH'] = os.environ['PATH']
        fixture.publish(runnable=True, matches=False, recipe_style='none')
        spec = importlib.util.spec_from_file_location('live_source_adapter', audit_test.ROOT / 'src/integrations/audit.py')
        adapter = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(adapter)
        agent = adapter.agents()[0]
        self.assertNotEqual(agent, 'manual')
        result = subprocess.run([str(audit_test.BINARY), '--home', str(fixture.home),
            'install', fixture.package, '--from-source', '--audit-agent', agent, '--json'],
            env=fixture.env, capture_output=True, text=True, timeout=360)
        self.assertEqual(result.returncode, 0, (result.stdout, result.stderr))
        identity = json.loads(result.stdout)['release']['identity']
        info = json.loads(fixture.run_cli('info', identity, '--json').stdout)
        evidence = info['audits'][0]['evidence']
        self.assertFalse(evidence['binary_match'])
        self.assertIn('SHA-256', evidence['provenance'])
        self.assertNotEqual(info['publisher'], fixture.author)
        self.assertEqual(fixture.run_cli('run', 'hello').stdout.strip(), 'Haps audit test passed')
        self.assertFalse((fixture.root / 'agent-called').exists())
        if path := os.environ.get('HAPS_LIVE_SOURCE_EVIDENCE'):
            Path(path).write_text(json.dumps({'status': 'passed', 'agent': agent,
                'evidence': evidence, 'installed_command_output': 'Haps audit test passed',
                'publisher_binary_mismatched': True}, indent=2) + '\n')

    def test_default_agent_reviews_rebuilds_installs_and_runs(self):
        fixture = audit_test.Audits('runTest')
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        # Keep the real Omarchy PATH/defaults and actual installed agent. The
        # ordinary offline suite's fake executable must never enter this test.
        fixture.env['PATH'] = os.environ['PATH']
        fixture.publish(runnable=True)
        denied = fixture.run_cli('install', fixture.package, success=False)
        self.assertIn('audit(s)', denied.stderr)
        spec = importlib.util.spec_from_file_location('live_audit_adapter',
            audit_test.ROOT / 'src/integrations/audit.py')
        adapter = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(adapter)
        agents = adapter.agents()
        self.assertNotEqual(agents[0], 'manual', 'a real agent must be installed')
        executable = adapter.executable(agents[0])
        self.assertIsNotNone(executable)
        self.assertNotEqual(Path(executable).parent, fixture.bin)

        class Environment:
            def args(self, _, *args):
                return [str(audit_test.BINARY), '--home', str(fixture.home), *args]
            def env(self, _):
                return {**fixture.env, 'HAPS_NON_INTERACTIVE': 'false', 'TERM': 'xterm-256color'}

        terminal = Terminal(Environment(), 'install', 'hello')
        self.addCleanup(terminal.close)
        terminal.read_until(b'Install this package?')
        terminal.send(b'y')
        terminal.read_until(b'Audit and install')
        terminal.send(b'y')
        terminal.read_until(b'Choose how to audit')
        terminal.send(b'\n')
        terminal.read_until(b'Publish this and future audits', timeout=240)
        terminal.send(b'n')
        code, output = terminal.finish()
        self.assertEqual(code, 0, output)
        self.assertIn('Rebuilt payload matches', output)
        info = json.loads(fixture.run_cli('info', 'hello', '--json').stdout)
        evidence = info['audits'][0]['evidence']
        self.assertTrue(evidence['binary_match'])
        self.assertEqual(evidence['reviewer']['agent'], agents[0])
        self.assertEqual(fixture.run_cli('run', 'hello').stdout.strip(), 'Haps audit test passed')
        self.assertFalse((fixture.home / 'discovery/outbox').exists())
        self.assertEqual(list((fixture.home / 'audits/work').iterdir()), [])
        self.assertFalse((fixture.root / 'agent-called').exists())
        if path := os.environ.get('HAPS_LIVE_EVIDENCE'):
            Path(path).write_text(json.dumps({'status': 'passed', 'release_id': fixture.release['id'],
                'agent': agents[0], 'evidence': evidence, 'installed_command_output': 'Haps audit test passed',
                'public_audit_published': False, 'temporary_checkout_cleaned': True}, indent=2) + '\n')


if __name__ == '__main__':
    unittest.main()
