import importlib.util
import json
import os
from pathlib import Path
import subprocess
import shlex
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('omarchy', Path(__file__).parents[1] / 'src/integrations/omarchy.py')
omarchy = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(omarchy)

class MenuTests(unittest.TestCase):
    def test_search_reopens_saved_results_refreshes_and_invalidates_changed_trust(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            row = {'identity': 'a' * 64 + '/app', 'label': 'publisher/app',
                   'publisher': 'a' * 64, 'release_id': 'b' * 64,
                   'package': {'version': '1.0.0', 'target': 'test-linux'}}
            (root / 'rows').write_text(json.dumps([row]))
            executable = root / 'haps'
            executable.write_text('''#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
root = Path(sys.argv[2])
args = sys.argv[3:]
with (root / 'calls').open('a') as file:
    file.write(json.dumps(args) + '\\n')
if args[0] == 'target':
    print('test-linux')
elif args[0] == 'search':
    (root / 'search.pid').write_text(str(os.getpid()))
    time.sleep(float(os.environ.get('SEARCH_DELAY', '0')))
    if os.environ.get('SEARCH_FAIL'):
        sys.exit(7)
    print((root / 'rows').read_text())
''')
            executable.chmod(0o755)
            picker = root / 'fzf'
            picker.write_text('''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
root = Path(os.environ['HOME'])
rows = sys.stdin.read().splitlines()
(root / 'shown').write_text(json.dumps(rows))
(root / 'header').write_text(next(arg for arg in sys.argv if arg.startswith('--header=')))
if (root / 'refresh').exists():
    (root / 'refresh').unlink()
    print('ctrl-r')
elif os.environ.get('PICK_INSTALL'):
    print('')  # fzf --expect emits an empty key line for Enter.
    print(rows[0])
else:
    sys.exit(130)
''')
            picker.chmod(0o755)
            env = dict(os.environ, HOME=tmp, PATH=tmp + os.pathsep + os.environ['PATH'])

            def run(**extra):
                (root / 'calls').unlink(missing_ok=True)
                result = subprocess.run(
                    [sys.executable, '-c', omarchy.PACKAGE_PICKER,
                     'search', str(executable), '--home', tmp],
                    env=dict(env, **extra), text=True, capture_output=True, timeout=5)
                calls = [json.loads(line) for line in (root / 'calls').read_text().splitlines()]
                return result, calls

            result, calls = run()
            self.assertEqual(result.returncode, 130, result.stderr)
            self.assertEqual(calls, [['target'], ['search', '--json', '--', '']])
            result, calls = run(SEARCH_FAIL='1', PICK_INSTALL='1')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls, [['target'], ['install', '--version', '1.0.0', '--', row['identity']]])
            self.assertIn('Saved list from', (root / 'header').read_text())
            self.assertIn('Ctrl-R: refresh', (root / 'header').read_text())

            row['package']['version'] = '2.0.0'
            (root / 'rows').write_text(json.dumps([row]))
            (root / 'refresh').touch()
            result, calls = run(PICK_INSTALL='1')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls, [['target'], ['search', '--json', '--', ''],
                                     ['install', '--version', '2.0.0', '--', row['identity']]])

            (root / 'refresh').touch()
            result, calls = run(SEARCH_FAIL='1')
            self.assertEqual(result.returncode, 130, result.stderr)
            self.assertIn('Refresh failed', result.stderr)
            self.assertIn('showing previous list', (root / 'header').read_text())
            self.assertIn('2.0.0', (root / 'shown').read_text())

            for filename in ('social.json', 'config.json'):
                (root / filename).write_text('{}')
                result, calls = run()
                self.assertEqual(result.returncode, 130, result.stderr)
                self.assertEqual(calls, [['target'], ['search', '--json', '--', '']])

            (root / 'integrations/omarchy-search.json').write_text('{broken')
            result, calls = run()
            self.assertEqual(result.returncode, 130, result.stderr)
            self.assertEqual(calls, [['target'], ['search', '--json', '--', '']])

            (root / 'rows').write_text('[]')
            (root / 'refresh').touch()
            result, calls = run()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('No packages found', result.stdout)
            result, calls = run()
            self.assertEqual(calls, [['target'], ['search', '--json', '--', '']])

    def test_slow_search_reports_progress_and_timeout_reaps_child(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            executable = root / 'haps'
            executable.write_text('''#!/usr/bin/env python3
import os, sys, time
from pathlib import Path
if sys.argv[1] == 'target':
    print('test-linux')
else:
    Path(os.environ['HOME'], 'search.pid').write_text(str(os.getpid()))
    time.sleep(30)
''')
            executable.chmod(0o755)
            script = omarchy.PACKAGE_PICKER.replace('SEARCH_POLL_SECONDS = 5', 'SEARCH_POLL_SECONDS = 0.1')
            script = script.replace('SEARCH_TIMEOUT_SECONDS = 120', 'SEARCH_TIMEOUT_SECONDS = 0.4')
            started = time.monotonic()
            result = subprocess.run([sys.executable, '-c', script, 'search', str(executable)],
                                    env=dict(os.environ, HOME=tmp), text=True, capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 1)
            self.assertIn('Still finding packages', result.stderr)
            self.assertIn('Package search timed out', result.stderr)
            self.assertLess(time.monotonic() - started, 5)
            with self.assertRaises(ProcessLookupError):
                os.kill(int((root / 'search.pid').read_text()), 0)

    def test_search_results_are_readable_and_strip_terminal_controls(self):
        rows = [{'label': 'publisher/app', 'package': {'version': '1.2.3', 'description': 'Hello\x1b[2J\nworld', 'target': 'x86_64-unknown-linux-gnu'}, 'audits': [{}], 'warnings': []}]
        result = subprocess.run([sys.executable, '-c', omarchy.SEARCH_FORMATTER], input=json.dumps(rows), text=True, capture_output=True, check=True)
        self.assertIn('publisher/app  1.2.3', result.stdout)
        self.assertIn('Audits: 1 · Warnings: 0', result.stdout)
        self.assertNotIn('\x1b', result.stdout)
        self.assertNotIn('"package"', result.stdout)
        empty = subprocess.run([sys.executable, '-c', omarchy.SEARCH_FORMATTER], input='[]', text=True, capture_output=True, check=True)
        self.assertIn('No matching packages', empty.stdout)
        installed = subprocess.run([sys.executable, '-c', omarchy.SEARCH_FORMATTER, 'list'], input='[]', text=True, capture_output=True, check=True)
        self.assertIn('No installed packages.', installed.stdout)

    def test_menu_requires_existing_haps_without_bootstrapping(self):
        with patch.object(omarchy, 'find_haps', return_value=None), \
             patch.object(omarchy.subprocess, 'call') as call:
            with self.assertRaisesRegex(ValueError, 'Install Haps'):
                omarchy.terminal_menu()
            call.assert_not_called()

    def test_menu_uses_existing_haps_without_installing_or_changing_menu(self):
        with patch.object(omarchy, 'find_haps', return_value='/some path/haps'), \
             patch.object(omarchy.shutil, 'which', return_value='/usr/bin/fzf'), \
             patch.object(omarchy.subprocess, 'call', return_value=0) as call, \
             patch('builtins.input') as prompt:
            self.assertEqual(omarchy.terminal_menu(), 0)
            prompt.assert_not_called()
            argv = call.call_args.args[0]
            self.assertEqual(argv[:2], ['bash', '-c'])
            self.assertEqual(argv[3:], ['haps-menu', 'menu'])
            self.assertIn("haps=('/some path/haps')", argv[2])
            self.assertNotIn('--home', argv[2].splitlines()[2])

    def test_menu_finds_existing_install_outside_desktop_path(self):
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

    def test_menu_actions_preserve_arguments_and_failures(self):
        with tempfile.TemporaryDirectory(prefix="haps menu ' $ ") as tmp:
            root = Path(tmp)
            row = {'identity': 'a' * 64 + '/app', 'label': 'publisher/app',
                   'publisher': 'a' * 64, 'release_id': 'b' * 64,
                   'package': {'version': '1.0.0', 'target': 'test-linux', 'description': 'An app'}}
            executable = root / 'haps'
            executable.write_text('#!/usr/bin/env python3\nimport json, os, sys\nfrom pathlib import Path\np=Path(os.environ["HOME"]) / "calls"\nwith p.open("a") as f: f.write(json.dumps(sys.argv[1:]) + "\\n")\nif sys.argv[1] in ("list", "search"): print(os.environ["HAPS_TEST_ROWS"])\nif sys.argv[1] == "target": print("test-linux")\nsys.exit(int(os.environ.get("HAPS_TEST_STATUS", "0")))\n')
            executable.chmod(0o755)
            picker = root / 'fzf'
            picker.write_text('#!/usr/bin/env python3\nimport os, sys\nrows=sys.stdin.read().splitlines()\nselection=os.environ.get("HAPS_TEST_PICK", "")\nif selection == "cancel": sys.exit(130)\nprint(next((row for row in rows if row.split("\\t")[0] == selection), rows[0]))\n')
            picker.chmod(0o755)
            env = dict(os.environ, HOME=tmp, PATH=tmp + os.pathsep + os.environ['PATH'], HAPS_TEST_ROWS=json.dumps([row]))
            calls = root / 'calls'
            def run(action, answer='\n', **extra):
                calls.unlink(missing_ok=True)
                result = subprocess.run(['bash', '-c', omarchy.menu_script(str(executable)), 'haps-menu', action], input=answer, text=True, capture_output=True, env=dict(env, **extra))
                args = [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []
                return result.returncode, args
            package = '--allow-unaudited; $(touch should-not-exist)'
            self.assertEqual(run('menu', package + '\n\n', HAPS_TEST_PICK='install'), (0, [['install', '--', package]]))
            self.assertEqual(run('search', HAPS_TEST_STATUS='7'), (7, [['target']]))
            self.assertEqual(run('search'), (0, [['target'], ['search', '--json', '--', ''], ['install', '--version', '1.0.0', '--', row['identity']]]))
            self.assertEqual(run('update'), (0, [['list', '--json'], ['update', '--', row['identity']]]))
            self.assertEqual(run('update', HAPS_TEST_STATUS='7'), (7, [['list', '--json']]))
            self.assertEqual(run('remove'), (0, [['list', '--json'], ['remove', '--', row['identity']]]))
            self.assertEqual(run('remove', HAPS_TEST_PICK='cancel'), (0, [['list', '--json']]))
            self.assertEqual(run('search', HAPS_TEST_ROWS='[]'), (0, [['target'], ['search', '--json', '--', '']]))
            self.assertEqual(run('menu', HAPS_TEST_PICK='cancel'), (0, []))
            self.assertEqual(run('install', ''), (0, []))
            self.assertEqual(run('unknown'), (2, []))

    def test_picker_filters_platform_and_keeps_display_text_out_of_commands(self):
        with tempfile.TemporaryDirectory(prefix="haps picker ' $ ") as tmp:
            root = Path(tmp)
            rows = [
                {'identity': 'a' * 64 + '/app', 'label': 'publisher/app', 'publisher': 'a' * 64,
                 'release_id': 'b' * 64, 'package': {'version': '1.0.0', 'target': 'other-platform'}},
                {'identity': 'c' * 64 + '/app', 'label': '$(touch bad)\t\x1b[2J', 'publisher': 'c' * 64,
                 'release_id': 'd' * 64, 'package': {'version': '2.0.0', 'target': 'test-linux', 'description': 'line1\nline2'},
                 'audits': [{}], 'warnings': [{}]},
            ]
            # Discovery includes history and prereleases. The default picker shows
            # the newest stable version per publisher, like the CLI's selection.
            for version in ['1.10.0', '1.9.0', '3.0.0-beta.1']:
                older = dict(rows[1], package=dict(rows[1]['package'], version=version))
                rows.append(older)
            executable = root / 'haps'
            executable.write_text('#!/usr/bin/env python3\nimport json, sys\nfrom pathlib import Path\nargs=sys.argv[3:]\nroot=Path(sys.argv[2])\nif args[0]=="search": print((root/"rows").read_text())\nelif args[0]=="target": print("test-linux")\nelse: (root/"installed").write_text(json.dumps(args)); sys.exit(8)\n')
            executable.chmod(0o755)
            picker = root / 'fzf'
            picker.write_text('''#!/usr/bin/env python3
import json, os, shlex, subprocess, sys
from pathlib import Path
root=Path(os.environ['HOME'])
rows=sys.stdin.read().splitlines()
(root/'choices').write_text(json.dumps(rows))
preview=next(arg.removeprefix('--preview=') for arg in sys.argv if arg.startswith('--preview='))
preview=shlex.split(preview.replace('{1}', '0'))
(root/'preview-path').write_text(preview[1])
(root/'preview').write_text(subprocess.check_output(preview, text=True))
print(rows[0])
''')
            picker.chmod(0o755)
            (root / 'rows').write_text(json.dumps(rows))
            env = dict(os.environ, HOME=tmp, PATH=tmp + os.pathsep + os.environ['PATH'])
            result = subprocess.run([sys.executable, '-c', omarchy.PACKAGE_PICKER, 'search', str(executable), '--home', tmp], env=env, text=True, capture_output=True)
            self.assertEqual(result.returncode, 8, result.stderr)
            self.assertEqual(json.loads((root / 'installed').read_text()), ['install', '--version', '2.0.0', '--', rows[1]['identity']])
            choices = json.loads((root / 'choices').read_text())
            self.assertEqual(len(choices), 1)
            self.assertEqual(len(choices[0].split('\t')), 4)
            self.assertNotIn('\x1b', choices[0])
            self.assertIn('Audits: 1\nWarnings: 1', (root / 'preview').read_text())
            self.assertIn(rows[1]['release_id'], (root / 'preview').read_text())
            self.assertFalse(Path((root / 'preview-path').read_text()).exists())
            self.assertFalse((root / 'bad').exists())

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
        for key in ['install.haps', 'remove.haps', 'update.haps.custom']:
            with self.assertRaises(ValueError):
                omarchy.merge(json.dumps({key: {'label': 'Mine'}}), {'install.haps': {}})
        with self.assertRaises(ValueError):
            omarchy.merge('{not json}', {'haps': {}})

    def test_migrates_owned_top_level_menu_without_changing_custom_entries(self):
        raw = '{\n"personal":{"label":"Notes"}\n}\n'
        old = omarchy.merge(raw, {'haps': {'label': 'Haps'}, 'haps.search': {'action': 'old'}})
        updated = omarchy.merge(old, {'install.haps': {'label': 'Haps', 'action': 'new'}})
        self.assertNotIn('"haps.search"', updated)
        self.assertNotIn('"haps":', updated)
        self.assertEqual(omarchy.merge(updated, {}), raw)

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
                for parent, action in [('install', 'search'), ('update', 'update'), ('remove', 'remove')]:
                    row = omarchy.parse(first)[1][parent + '.haps']
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
