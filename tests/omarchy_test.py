import importlib.util
import json
import os
from pathlib import Path
import subprocess
import shlex
import sys
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('omarchy', Path(__file__).parents[1] / 'src/integrations/omarchy.py')
omarchy = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(omarchy)

class MenuTests(unittest.TestCase):
    def test_search_results_are_readable_and_strip_terminal_controls(self):
        rows = [{'label': 'publisher/app', 'package': {'version': '1.2.3', 'description': 'Hello\x1b[2J\nworld', 'target': 'x86_64-unknown-linux-gnu'}, 'audits': [{}], 'warnings': []}]
        result = subprocess.run([sys.executable, '-c', omarchy.SEARCH_FORMATTER], input=json.dumps(rows), text=True, capture_output=True, check=True)
        self.assertIn('publisher/app  1.2.3', result.stdout)
        self.assertIn('Audits: 1 · Warnings: 0', result.stdout)
        self.assertNotIn('\x1b', result.stdout)
        self.assertNotIn('"package"', result.stdout)
        empty = subprocess.run([sys.executable, '-c', omarchy.SEARCH_FORMATTER], input='[]', text=True, capture_output=True, check=True)
        self.assertIn('No matching packages', empty.stdout)

    def test_plugin_launch_quotes_paths_for_omarchy_terminal(self):
        with patch.object(omarchy.subprocess, 'call', return_value=0) as call:
            omarchy.launch_plugin()
        command = call.call_args.args[0]
        self.assertEqual(command[:2], ['omarchy-launch-or-focus-tui', '--app-id=to.hashtree.haps'])
        self.assertEqual(shlex.split(command[2]), [sys.executable, str(Path(omarchy.__file__).resolve()), '--menu'])

    def test_plugin_setup_decline_or_eof_never_installs(self):
        for answer in ['n', '', 'yes please', EOFError()]:
            with patch.object(omarchy, 'find_haps', return_value=None), \
                 patch('builtins.input', side_effect=answer if isinstance(answer, Exception) else [answer]), \
                 patch.object(omarchy.subprocess, 'call') as call:
                self.assertEqual(omarchy.plugin_menu(), 0)
                call.assert_not_called()

    def test_plugin_setup_uses_bundled_installer_and_stops_on_failure(self):
        with tempfile.TemporaryDirectory() as tmp, \
             patch.dict(os.environ, {'HOME': tmp, 'HAPS_INSTALL_DIR': tmp + '/custom bin'}), \
             patch.object(omarchy, 'find_haps', return_value=None), \
             patch('builtins.input', return_value='y'), \
             patch.object(omarchy.subprocess, 'call', return_value=9) as call:
            self.assertEqual(omarchy.plugin_menu(), 9)
            call.assert_called_once_with(['sh', str(Path(__file__).parents[1] / 'website/public/install.sh'), '--bin-dir', tmp + '/custom bin'])

    def test_plugin_uses_existing_haps_without_installing_or_changing_menu(self):
        with patch.object(omarchy, 'find_haps', return_value='/some path/haps'), \
             patch.object(omarchy.shutil, 'which', return_value='/usr/bin/fzf'), \
             patch.object(omarchy.subprocess, 'call', return_value=0) as call, \
             patch('builtins.input') as prompt:
            self.assertEqual(omarchy.plugin_menu(), 0)
            prompt.assert_not_called()
            argv = call.call_args.args[0]
            self.assertEqual(argv[:2], ['bash', '-c'])
            self.assertEqual(argv[3:], ['haps-menu', 'menu'])
            self.assertIn("haps=('/some path/haps')", argv[2])
            self.assertNotIn('--home', argv[2])

    def test_plugin_successful_setup_launches_verified_command(self):
        with tempfile.TemporaryDirectory() as tmp, \
             patch.dict(os.environ, {'HOME': tmp, 'HAPS_INSTALL_DIR': tmp + '/local bin'}), \
             patch.object(omarchy, 'find_haps', return_value=None), \
             patch.object(omarchy.shutil, 'which', return_value='/usr/bin/fzf'), \
             patch('builtins.input', return_value='yes'):
            executable = Path(tmp) / 'local bin/haps'
            def call(argv):
                if argv[0] == 'sh':
                    executable.parent.mkdir()
                    executable.write_text('#!/bin/sh\nexit 0\n')
                    executable.chmod(0o755)
                return 0
            with patch.object(omarchy.subprocess, 'call', side_effect=call) as launch:
                self.assertEqual(omarchy.plugin_menu(), 0)
            self.assertEqual(launch.call_count, 2)
            self.assertIn(shlex.quote(str(executable)), launch.call_args.args[0][2])

    def test_plugin_finds_existing_install_outside_desktop_path(self):
        with tempfile.TemporaryDirectory() as tmp, \
             patch.dict(os.environ, {'HOME': tmp, 'HAPS_INSTALL_DIR': tmp + '/local bin'}), \
             patch.object(omarchy.shutil, 'which', return_value=None):
            executable = Path(tmp) / 'local bin/haps'
            executable.parent.mkdir()
            executable.write_text('#!/bin/sh\nexit 0\n')
            executable.chmod(0o644)
            self.assertIsNone(omarchy.find_haps())
            executable.chmod(0o755)
            self.assertEqual(omarchy.find_haps(), str(executable))
            with patch.object(omarchy.shutil, 'which', return_value='/preferred/haps'):
                self.assertEqual(omarchy.find_haps(), '/preferred/haps')
            with patch.dict(os.environ, {'HAPS_INSTALL_DIR': ''}):
                self.assertEqual(omarchy.install_directory(), Path(tmp) / '.local/bin')

    def test_plugin_actions_preserve_arguments_and_failures(self):
        with tempfile.TemporaryDirectory(prefix="haps plugin ' $ ") as tmp:
            root = Path(tmp)
            executable = root / 'haps'
            executable.write_text('#!/usr/bin/env python3\nimport json, os, sys\nfrom pathlib import Path\np=Path(os.environ["HOME"]) / "calls"\nwith p.open("a") as f: f.write(json.dumps(sys.argv[1:]) + "\\n")\nif sys.argv[1] == "list": print("publisher/app\\t1.0.0")\nsys.exit(int(os.environ.get("HAPS_TEST_STATUS", "0")))\n')
            executable.chmod(0o755)
            picker = root / 'fzf'
            picker.write_text('#!/usr/bin/env python3\nimport os, sys\nrows=sys.stdin.read().splitlines()\nselection=os.environ.get("HAPS_TEST_PICK", "")\nif selection == "cancel": sys.exit(130)\nprint(next((row for row in rows if row.split("\\t")[0] == selection), rows[0]))\n')
            picker.chmod(0o755)
            env = dict(os.environ, HOME=tmp, PATH=tmp + os.pathsep + os.environ['PATH'])
            calls = root / 'calls'
            def run(action, answer='\n', **extra):
                calls.unlink(missing_ok=True)
                result = subprocess.run(['bash', '-c', omarchy.menu_script(str(executable)), 'haps-menu', action], input=answer, text=True, capture_output=True, env=dict(env, **extra))
                args = [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []
                return result.returncode, args
            package = '--allow-unaudited; $(touch should-not-exist)'
            self.assertEqual(run('menu', package + '\n\n', HAPS_TEST_PICK='install'), (0, [['install', '--', package]]))
            self.assertEqual(run('search', package + '\n\n', HAPS_TEST_STATUS='7'), (7, [['search', '--json', '--', package]]))
            self.assertEqual(run('update'), (0, [['list'], ['update', '--', 'publisher/app']]))
            self.assertEqual(run('update', HAPS_TEST_STATUS='7'), (7, [['list']]))
            self.assertEqual(run('remove'), (0, [['list'], ['remove', '--', 'publisher/app']]))
            self.assertEqual(run('remove', HAPS_TEST_PICK='cancel'), (0, [['list']]))
            self.assertEqual(run('menu', HAPS_TEST_PICK='cancel'), (0, []))
            self.assertEqual(run('install', ''), (0, []))
            self.assertEqual(run('unknown'), (2, []))

    def test_preserves_custom_menu_and_comments_and_is_idempotent(self):
        for raw in ['{\n  // Personal notes\n  "personal": {"label":"Notes"},\n}\n', '{\n"items": {\n"personal": {"label":"Notes"}\n}\n}\n']:
            rows = {'haps': {'label': 'Haps'}, 'haps.search': {'action': 'example'}}
            result = omarchy.merge(raw, rows)
            self.assertEqual(omarchy.merge(result, rows), result)
            self.assertEqual(omarchy.merge(result, {}), raw)
            self.assertIn('"personal": {"label":"Notes"}', result)
            parsed = omarchy.parse(result)[1]
            self.assertIn('haps', parsed.get('items', parsed))
        with self.assertRaises(ValueError):
            omarchy.merge('{"haps":{"label":"Mine"}}', {'haps': {}})
        with self.assertRaises(ValueError):
            omarchy.merge('{not json}', {'haps': {}})

    def test_install_run_update_and_remove_keep_user_configuration(self):
        with tempfile.TemporaryDirectory(prefix='haps menu $ ') as tmp:
            root = Path(tmp)
            menu = root / '.config/omarchy/extensions/omarchy-menu.jsonc'
            menu.parent.mkdir(parents=True)
            original = '{\n // keep me\n "personal": {"label":"Notes"}\n}\n'
            menu.write_text(original)
            executable = root / 'fake haps'
            executable.write_text('#!/usr/bin/env bash\nprintf "%s\\n" "$@" > "$HOME/args"\n')
            executable.chmod(0o755)
            home = root / 'data'
            with patch.dict(os.environ, {'HOME': str(root)}):
                omarchy.install(str(executable), str(home))
                first = menu.read_text()
                for action in ['search', 'install', 'list', 'update', 'remove']:
                    row = omarchy.parse(first)[1]['haps.' + action]
                    command = shlex.split(row['action'])
                    self.assertEqual(command[:2], ['omarchy-launch-or-focus-tui', '--app-id=to.hashtree.haps.' + action])
                    self.assertEqual(shlex.split(command[2]), [str(home / 'integrations/omarchy-menu'), action])
                omarchy.install(str(executable), str(home))
                self.assertEqual(first, menu.read_text())
                helper = home / 'integrations/omarchy-menu'
                subprocess.run(['bash', str(helper), 'install'], input='siriusbusiness/iris-chat\n\n', text=True, check=True)
                self.assertEqual((root / 'args').read_text().splitlines(), ['--home', str(home), 'install', '--', 'siriusbusiness/iris-chat'])
                omarchy.install(str(executable), str(home), remove=True)
                self.assertFalse(helper.exists())
                self.assertEqual(menu.read_text(), original)
                self.assertEqual(menu.with_suffix('.jsonc.haps-backup').read_text(), original)

if __name__ == '__main__':
    unittest.main()
