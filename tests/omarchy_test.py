import importlib.util
import fcntl
import struct
import json
import shutil
import os
import pty
import select
import signal
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
            self.assertEqual(run('update'), (0, [['list', '--json'], ['update', '--', row['identity']]]))
            self.assertEqual(run('update', HAPS_TEST_STATUS='7'), (7, [['list', '--json']]))
            self.assertEqual(run('remove'), (0, [['list', '--json'], ['remove', '--', row['identity']]]))
            self.assertEqual(run('remove', HAPS_TEST_PICK='cancel'), (0, [['list', '--json']]))
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



def package(version='1.0.0', release='b', publisher='a', **changes):
    row = {'identity': publisher * 64 + '/app', 'label': 'publisher/app',
           'publisher': publisher * 64, 'release_id': release * 64,
           'package': {'version': version, 'target': 'test-linux', 'description': 'An app'}}
    row.update(changes)
    return row


SEARCH_STUB = r'''#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
root = Path(os.environ['HOME'])
args = sys.argv[1:]
with (root / 'calls').open('a') as file:
    file.write(json.dumps(args) + '\n')
if args[0] == 'target':
    print('test-linux')
elif args[0] == 'search':
    with (root / 'search-pids').open('a') as file:
        file.write(str(os.getpid()) + '\n')
    run = len((root / 'search-pids').read_text().splitlines()) - 1
    scenarios = json.loads((root / 'scenarios').read_text())
    scenario = scenarios[min(run, len(scenarios) - 1)]
    for frame in scenario.get('frames', []):
        if 'wait' in frame:
            while not (root / frame['wait']).exists():
                time.sleep(0.01)
        time.sleep(frame.get('delay', 0))
        if 'raw' in frame:
            print(frame['raw'], end='', flush=True)
        else:
            print(json.dumps(frame['rows']), flush=True)
    if scenario.get('close_stdout'):
        os.close(1)
    if scenario.get('hold'):
        while True: time.sleep(1)
    sys.exit(scenario.get('code', 0))
else:
    (root / 'installed').write_text(json.dumps(args))
    sys.exit(int(os.environ.get('INSTALL_STATUS', '0')))
'''


PICKER_STUB = r'''#!/usr/bin/env python3
from http.server import BaseHTTPRequestHandler, HTTPServer
import json, os, re, shlex, subprocess, sys, time
from pathlib import Path
if '--help' in sys.argv:
    print(os.environ.get('FZF_TEST_HELP', '--id-nth'))
    sys.exit(0)
root = Path(os.environ['HOME'])
work = Path(os.environ['HAPS_PICKER_DIR'])
with (root / 'picker-pids').open('a') as file:
    file.write(str(os.getpid()) + '\n')
run = len((root / 'picker-pids').read_text().splitlines()) - 1
(root / 'opened').touch()
rows, first, headers = [], None, []
class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        global rows, first
        assert self.headers['x-api-key'] == os.environ['FZF_API_KEY']
        actions = self.rfile.read(int(self.headers['Content-Length'])).decode()
        for name in re.findall(r'\$HAPS_PICKER_DIR/(snapshot-\d+)', actions):
            rows = (work / name).read_text().splitlines()
            if first is None and rows: first = rows[0]
            with (root / 'shown').open('a') as file:
                file.write(json.dumps(rows) + '\n')
        headers.append(actions)
        (root / 'headers').write_text('\n'.join(headers))
        self.send_response(200)
        self.end_headers()
    def log_message(self, *args): pass
server = HTTPServer(('127.0.0.1', 0), Handler)
server.timeout = 0.01
(work / 'port').write_text(str(server.server_port))
mode = os.environ.get('PICKER_MODE', 'first')
while True:
    server.handle_request()
    if mode == 'cancel' and (root / 'cancel').exists(): sys.exit(130)
    if mode == 'timeout' and any('timed out' in h for h in headers): sys.exit(130)
    if mode == 'empty' and any('No packages found' in h for h in headers): sys.exit(130)
    if mode == 'failure' and any('search failed' in h for h in headers): sys.exit(130)
    if mode == 'stale' and first and rows and first != rows[0]:
        print('\n\n' + first)
        break
    if mode == 'refresh' and rows and run == 0:
        print('app\nctrl-r\n' + rows[0])
        break
    if rows and (mode in ('first', 'refresh', 'forged') or (mode == 'wait' and (root / 'pick').exists())):
        row = rows[0]
        preview = next(arg[10:] for arg in sys.argv if arg.startswith('--preview='))
        command = preview.replace('{1}', shlex.quote(row.split('\t')[0]))
        (root / 'preview').write_text(subprocess.check_output(['sh', '-c', command], text=True))
        (root / 'preview-dir').write_text(str(work))
        print('\n\n' + (row + ' forged' if mode == 'forged' else row))
        break
server.server_close()
'''


class PickerFixture(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="haps picker ' $ ")
        self.root = Path(self.temp.name)
        self.haps = self.root / 'haps'
        self.haps.write_text(SEARCH_STUB)
        self.haps.chmod(0o755)
        fzf = self.root / 'fzf'
        fzf.write_text(PICKER_STUB)
        fzf.chmod(0o755)
        self.env = dict(os.environ, HOME=str(self.root), PATH=str(self.root) + os.pathsep + os.environ['PATH'])
        self.children = []

    def tearDown(self):
        for child in self.children:
            if child.poll() is None:
                child.kill()
            child.communicate()
        self.temp.cleanup()

    def start(self, scenarios, mode='first', script=None, **extra):
        (self.root / 'scenarios').write_text(json.dumps(scenarios))
        child = subprocess.Popen([sys.executable, '-c', script or omarchy.PACKAGE_PICKER,
                                  'search', str(self.haps)], env=dict(self.env, PICKER_MODE=mode, **extra),
                                 text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.children.append(child)
        return child

    def wait_for(self, condition):
        deadline = time.monotonic() + 5
        while not condition():
            if time.monotonic() > deadline:
                self.fail('Timed out waiting for picker state')
            time.sleep(0.01)

    def finish(self, child, expected=0):
        stdout, stderr = child.communicate(timeout=6)
        self.assertEqual(child.returncode, expected, stdout + stderr)
        for name in ('picker-pids', 'search-pids'):
            if (self.root / name).exists():
                for pid in (self.root / name).read_text().splitlines():
                    with self.assertRaises(ProcessLookupError):
                        os.kill(int(pid), 0)
        return stdout, stderr

    def shown(self):
        path = self.root / 'shown'
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []



class PickerTests(PickerFixture):
    def test_old_fzf_is_rejected_before_search_or_picker_starts(self):
        child = self.start([], FZF_TEST_HELP='--track')
        _, error = self.finish(child, 1)
        self.assertIn('fzf 0.71 or newer', error)
        self.assertNotIn('Traceback', error)
        self.assertFalse((self.root / 'search-pids').exists())
        self.assertFalse((self.root / 'picker-pids').exists())
        self.assertFalse((self.root / 'installed').exists())

    def test_opens_before_first_snapshot_and_selects_before_search_eof(self):
        row = package()
        child = self.start([{'frames': [{'wait': 'send', 'rows': []}, {'rows': [row]}], 'hold': True}])
        self.wait_for(lambda: (self.root / 'opened').exists())
        self.assertIsNone(child.poll())
        self.assertFalse((self.root / 'installed').exists())
        (self.root / 'send').touch()
        self.finish(child)
        self.assertEqual(json.loads((self.root / 'installed').read_text()),
                         ['install', '--version', '1.0.0', '--release', 'b' * 64, '--', row['identity']])
        calls = [json.loads(line) for line in (self.root / 'calls').read_text().splitlines()]
        self.assertEqual(calls[1], ['search', '--json', '--stream', '--', ''])
        self.assertFalse((self.root / 'integrations/omarchy-search.json').exists())

    def test_eof_does_not_block_picker_while_search_is_exiting(self):
        child = self.start([{'frames': [{'rows': [package()]}], 'close_stdout': True, 'hold': True}])
        self.finish(child)

    def test_snapshots_replace_reorder_remove_and_pin_same_version_release(self):
        old, other, new = package(), package(publisher='c'), package(release='d')
        child = self.start([{'frames': [{'rows': [old, other]}, {'wait': 'next', 'rows': [new]}], 'hold': True}], 'wait')
        self.wait_for(lambda: self.shown())
        first = self.shown()[-1]
        (self.root / 'next').touch()
        self.wait_for(lambda: self.shown()[-1] != first)
        self.assertEqual(len(self.shown()[-1]), 1)
        self.assertNotEqual(first[0].split('\t')[0], self.shown()[-1][0].split('\t')[0])
        (self.root / 'pick').touch()
        self.finish(child)
        self.assertEqual(json.loads((self.root / 'installed').read_text())[3:5], ['--release', 'd' * 64])

    def test_invalidated_or_forged_selection_cannot_install(self):
        for mode in ('stale', 'forged'):
            with self.subTest(mode=mode):
                child = self.start([{'frames': [{'rows': [package()]},
                                                {'delay': 0.2, 'rows': [package(release='d')]}], 'hold': True}], mode)
                _, stderr = self.finish(child, 1)
                self.assertIn('Package results changed', stderr)
                self.assertFalse((self.root / 'installed').exists())

    def test_empty_replacement_removes_previous_rows(self):
        child = self.start([{'frames': [{'rows': [package()]}, {'wait': 'clear', 'rows': []}]}], 'empty')
        self.wait_for(lambda: self.shown())
        (self.root / 'clear').touch()
        self.finish(child, 130)
        self.assertEqual(self.shown()[-1], [])
    def test_refresh_restarts_search_and_reaps_previous_process(self):
        child = self.start([{'frames': [{'rows': [package()]}], 'hold': True},
                            {'frames': [{'rows': [package('2.0.0', 'c')]}], 'hold': True}], 'refresh')
        self.finish(child)
        self.assertEqual(len((self.root / 'search-pids').read_text().splitlines()), 2)
        self.assertEqual(json.loads((self.root / 'installed').read_text())[2:6], ['2.0.0', '--release', 'c' * 64, '--'])

    def test_cancel_timeout_failure_and_malformed_stream_reap_processes(self):
        child = self.start([{'frames': [], 'hold': True}], 'cancel')
        self.wait_for(lambda: (self.root / 'search-pids').exists())
        (self.root / 'cancel').touch()
        self.finish(child, 130)
        child = self.start([{'frames': [], 'hold': True}], 'timeout',
                           script=omarchy.PACKAGE_PICKER.replace('SEARCH_TIMEOUT_SECONDS = 120', 'SEARCH_TIMEOUT_SECONDS = 0.2'))
        _, stderr = self.finish(child, 130)
        self.assertIn('timed out', stderr)
        child = self.start([{'frames': [], 'code': 7}], 'failure')
        _, stderr = self.finish(child, 130)
        self.assertIn('search failed', stderr)
        child = self.start([{'frames': [{'raw': '{not json}\n'}], 'hold': True}])
        self.finish(child, 1)
        self.assertFalse((self.root / 'installed').exists())

    def test_filters_versions_platform_and_sanitizes_display_without_commands(self):
        row = package('2.0.0', label='$(touch bad)\t\x1b[2J', audits=[{}], warnings=[{}])
        row['package']['description'] = 'line1\nline2'
        other = package('9.0.0', publisher='c')
        other['package']['target'] = 'other-platform'
        child = self.start([{'frames': [{'rows': [other, row, package('1.10.0'),
                                                 package('1.9.0'), package('3.0.0-beta.1')]}]}], INSTALL_STATUS='8')
        self.finish(child, 8)
        choices = self.shown()[-1]
        self.assertEqual(len(choices), 1)
        self.assertEqual(len(choices[0].split('\t')), 4)
        self.assertNotIn('\x1b', choices[0])
        preview = (self.root / 'preview').read_text()
        self.assertIn('Audits: 1\nWarnings: 1', preview)
        self.assertIn(row['release_id'], preview)
        self.assertFalse(Path((self.root / 'preview-dir').read_text()).exists())
        self.assertFalse((self.root / 'bad').exists())


@unittest.skipUnless(os.environ.get('HAPS_TEST_FZF') or shutil.which('fzf'), 'real fzf is not available')
class RealPickerTests(PickerFixture):
    def setUp(self):
        super().setUp()
        actual = os.environ.get('HAPS_TEST_FZF') or shutil.which('fzf')
        (self.root / 'fzf').unlink()
        (self.root / 'fzf').write_text(
            '#!/usr/bin/env python3\nimport json, os, sys\nfrom pathlib import Path\n'
            'with (Path(os.environ["HOME"]) / "fzf-args").open("a") as file:\n'
            '    file.write(json.dumps(sys.argv[1:]) + "\\n")\n'
            'os.execv(' + repr(actual) + ', [' + repr(actual) + '] + sys.argv[1:])\n')
        (self.root / 'fzf').chmod(0o755)
        self.pid, self.master = None, None
        self.output = bytearray()

    def tearDown(self):
        if self.pid is not None:
            try:
                os.kill(self.pid, signal.SIGINT)
                self.wait_terminal()
            except (ProcessLookupError, ChildProcessError):
                pass
        if self.master is not None:
            os.close(self.master)
        super().tearDown()

    def terminal(self, scenarios):
        (self.root / 'scenarios').write_text(json.dumps(scenarios))
        self.pid, self.master = pty.fork()
        if self.pid == 0:
            import termios
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 110, 0, 0))
            os.execvpe(sys.executable, [sys.executable, '-c', omarchy.PACKAGE_PICKER,
                                      'search', str(self.haps)], dict(self.env, TERM='xterm-256color', FZF_DEFAULT_OPTS=''))

    def read_terminal(self):
        if select.select([self.master], [], [], 0.1)[0]:
            try:
                self.output.extend(os.read(self.master, 65536))
            except OSError:
                pass

    def rendered(self, needle):
        deadline = time.monotonic() + 5
        while needle not in self.output:
            if time.monotonic() > deadline:
                self.fail('fzf did not render ' + repr(needle) + ': ' + repr(self.output[-1600:]))
            self.read_terminal()

    def wait_terminal(self):
        deadline = time.monotonic() + 5
        while True:
            done, status = os.waitpid(self.pid, os.WNOHANG)
            if done:
                self.pid = None
                return os.waitstatus_to_exitcode(status)
            if time.monotonic() > deadline:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
                self.pid = None
                self.fail('Terminal did not exit: ' + repr(self.output[-1600:]))
            self.read_terminal()

    def assert_search_reaped(self):
        for entry in (self.root / 'search-pids').read_text().splitlines():
            with self.assertRaises(ProcessLookupError):
                os.kill(int(entry), 0)

    def test_real_fzf_progressive_replacement_before_eof(self):
        old, new = package(), package('2.0.0', 'd', label='$(touch bad) app')
        self.terminal([{'frames': [
            {'wait': 'send', 'rows': []}, {'rows': [old]}, {'wait': 'next', 'rows': [new]}], 'hold': True}])
        self.rendered(b'Haps')
        self.assertFalse((self.root / 'installed').exists())
        (self.root / 'send').touch()
        self.rendered(b'1.0.0')
        (self.root / 'next').touch()
        self.rendered(b'2.0.0')
        os.write(self.master, b'\r')
        self.assertEqual(self.wait_terminal(), 0)
        self.assertEqual(json.loads((self.root / 'installed').read_text())[2:6], ['2.0.0', '--release', 'd' * 64, '--'])
        self.assertFalse((self.root / 'bad').exists())
        self.assert_search_reaped()

    def test_real_fzf_same_version_new_release_and_eof(self):
        self.terminal([{'frames': [{'rows': [package()]},
                                  {'wait': 'next', 'rows': [package(release='d', label='replacement/app')]}]}])
        self.rendered(b'publisher/app')
        (self.root / 'next').touch()
        self.rendered(b'replacement/app')
        os.write(self.master, b'\r')
        self.assertEqual(self.wait_terminal(), 0)
        self.assertEqual(json.loads((self.root / 'installed').read_text())[3:5], ['--release', 'd' * 64])
        self.assert_search_reaped()

    def test_real_fzf_empty_replacement_and_cancel(self):
        self.terminal([{'frames': [{'rows': [package()]}, {'wait': 'clear', 'rows': []}]}])
        self.rendered(b'1.0.0')
        (self.root / 'clear').touch()
        self.rendered(b'No packages found')
        os.write(self.master, b'\x1b')
        self.assertEqual(self.wait_terminal(), 130)
        self.assertFalse((self.root / 'installed').exists())
        self.assert_search_reaped()

    def test_real_fzf_refresh_after_failure_and_cancel_active_search(self):
        self.terminal([{'frames': [], 'code': 7},
                       {'frames': [{'rows': [package('2.0.0', 'd', label='needle/app')]}], 'hold': True}])
        self.rendered(b'search failed')
        os.write(self.master, b'needle\x12')
        self.rendered(b'2.0.0')
        self.assertEqual(len((self.root / 'search-pids').read_text().splitlines()), 2)
        launches = [json.loads(line) for line in (self.root / 'fzf-args').read_text().splitlines() if '--help' not in line]
        self.assertIn('--query=needle', launches[1])
        os.write(self.master, b'\x1b')
        self.assertEqual(self.wait_terminal(), 130)
        self.assert_search_reaped()


    def test_real_fzf_tracks_selection_when_snapshot_reorders(self):
        first = package(label='first/app')
        second = package('2.0.0', 'd', publisher='c', label='second/app')
        self.terminal([{'frames': [{'rows': [first, second]},
                                  {'wait': 'next', 'rows': [second, first]}], 'hold': True}])
        self.rendered(b'first/app')
        os.write(self.master, b'\x1b[B')
        # The preview release ID confirms which row is focused before replacement.
        self.rendered(b'd' * 40)
        self.output.clear()
        (self.root / 'next').touch()
        # Wait for the focused release preview, not just a row elsewhere in the list.
        self.rendered(b'd' * 40)
        os.write(self.master, b'\r')
        self.assertEqual(self.wait_terminal(), 0)
        self.assertEqual(json.loads((self.root / 'installed').read_text())[-1], second['identity'])
        self.assert_search_reaped()

    def test_real_fzf_tracks_filtered_selection_after_removal_and_metadata_change(self):
        outside = package(label='outside/app')
        first = package(publisher='b', label='needle first/app')
        second = package('2.0.0', 'd', publisher='c', label='needle second/app')
        changed = dict(second, package=dict(second['package'], description='Updated description'))
        self.terminal([{'frames': [{'rows': [outside, first, second]},
                                  {'wait': 'next', 'rows': [outside, changed]}], 'hold': True}])
        self.rendered(b'outside/app')
        os.write(self.master, b'needle')
        self.rendered(b'needle first/app')
        os.write(self.master, b'\x1b[B')
        self.rendered(b'd' * 40)
        self.output.clear()
        (self.root / 'next').touch()
        self.rendered(b'Updated description')
        self.rendered(b'd' * 40)
        os.write(self.master, b'\r')
        self.assertEqual(self.wait_terminal(), 0)
        self.assertEqual(json.loads((self.root / 'installed').read_text())[-1], second['identity'])
        self.assert_search_reaped()

    def check_cancel_helper(self, kind):
        cat = self.root / 'cat'
        cat.write_text('''#!/usr/bin/env python3
import os, sys, time
from pathlib import Path
path = Path(sys.argv[1])
root = Path(os.environ['HOME'])
hold = (path.name.startswith('snapshot-') if os.environ['CAT_HOLD'] == 'reload'
        else len(path.name) == 64)
if hold:
    (root / 'helper-pid').write_text(str(os.getpid()))
    while True: time.sleep(1)
else:
    sys.stdout.write(path.read_text())
''')
        cat.chmod(0o755)
        self.env['CAT_HOLD'] = kind
        self.terminal([{'frames': [{'rows': [package()]}], 'hold': True}])
        self.wait_for(lambda: self.read_terminal() or (self.root / 'helper-pid').exists())
        os.write(self.master, b'\x1b')
        self.assertEqual(self.wait_terminal(), 130)
        with self.assertRaises(ProcessLookupError):
            os.kill(int((self.root / 'helper-pid').read_text()), 0)
        self.assert_search_reaped()

    def test_real_fzf_cancel_reaps_reload_child(self):
        self.check_cancel_helper('reload')

    def test_real_fzf_cancel_reaps_preview_child(self):
        self.check_cancel_helper('preview')


if __name__ == '__main__':
    unittest.main()
