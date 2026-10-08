"""Exercise the real CLI through a terminal, with locally signed demo catalogs."""
import errno
import json
import os
from pathlib import Path
import select
import struct
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get('HAPS_TEST_BINARY', ROOT / 'target/debug/haps')).resolve()


class Catalog:
    def __init__(self, root):
        self.root = Path(root)
        self.keys = {}
        for name in ['reader', 'alice', 'buildbot', 'other', 'bob', 'carol', 'dave']:
            self.keys[name] = self.run(name, 'identity', 'init').stdout.strip()
        payload = self.root / 'payload'
        payload.mkdir()
        source = self.root / 'hello.rs'
        source.write_text('fn main() { println!("Hello from Haps"); }\n')
        subprocess.run(['rustc', str(source), '-o', str(payload / 'hello')], check=True)
        assert subprocess.check_output([str(payload / 'hello')], text=True).strip() == 'Hello from Haps'
        target = self.run('reader', 'target').stdout.strip()
        spec = self.root / 'haps.toml'
        spec.write_text(f'name="hello"\nversion="1.0.0"\ntarget="{target}"\ndescription="Demo greeting"\n[commands]\nhello="hello"\n')
        self.releases = {}
        for name in ['alice', 'buildbot', 'other']:
            repo = self.root / f'{name}-repo'
            self.releases[name] = json.loads(self.run(name, 'pack', str(spec), '--payload', str(payload), '--out', str(repo)).stdout)
            self.run('reader', 'source', 'add', name, str(repo), '--author', self.keys[name])
            self.run('reader', 'alias', 'add', name, self.keys[name])
        for name in ['alice', 'bob', 'carol', 'dave']:
            self.run('reader', 'follow', self.keys[name])
            if name != 'alice':
                self.run('reader', 'alias', 'add', name, self.keys[name])
        for name in ['bob', 'carol']:
            self.run(name, 'source', 'add', 'alice', str(self.root / 'alice-repo'), '--author', self.keys['alice'])
            self.run(name, 'alias', 'add', 'alice', self.keys['alice'])
            note = 'Built from source; tests pass.' if name == 'bob' else 'Ran hello on this platform.'
            event = json.loads(self.run(name, 'attest', 'alice/hello', '--version', '1.0.0', '--note', note, '--audited', '--provenance', 'Test fixture', '--json').stdout)
            saved = self.home(name) / 'attestations' / f'{event["id"]}.json'
            assert json.loads(saved.read_text()) == event
            self.run('reader', 'import', str(saved))
        follows = self.root / 'follows.json'
        for name in ['alice', 'bob', 'carol', 'dave']:
            self.run(name, 'follow', self.keys['buildbot'], '--export', str(follows))
            self.run('reader', 'import', str(follows))

    def home(self, name):
        return self.root / name

    def env(self, name):
        env = dict(os.environ, HAPS_NO_DEFAULTS='true', HTREE_CONFIG_DIR=str(self.home(name) / 'hashtree'), TERM='xterm-256color')
        env.pop('HAPS_NON_INTERACTIVE', None)
        return env

    def args(self, name, *args):
        return [str(BINARY), '--home', str(self.home(name)), *args]

    def run(self, name, *args, check=True):
        return subprocess.run(self.args(name, *args), env=self.env(name), text=True, capture_output=True, check=check, timeout=30)


class Terminal:
    def __init__(self, catalog, *args, columns=80, env_extra=None):
        import fcntl
        import pty
        import termios
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', 32, columns, 0, 0))
            os.execve(str(BINARY), catalog.args('reader', *args), dict(catalog.env('reader'), **(env_extra or {})))
        self.output = b''
        self.closed = False

    def read_until(self, expected, timeout=10):
        deadline = time.monotonic() + timeout
        while expected not in self.output:
            if time.monotonic() >= deadline:
                raise AssertionError(f'Terminal timed out: {self.output!r}')
            if not self.read(0.1):
                raise AssertionError(f'Terminal ended before {expected!r}: {self.output!r}')
        return self.output

    def read(self, timeout):
        if select.select([self.fd], [], [], timeout)[0]:
            try:
                data = os.read(self.fd, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                return False
            self.output += data
            return bool(data)
        return True

    def send(self, data):
        os.write(self.fd, data)

    def finish(self, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.read(0.1)
            pid, code = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                while self.read(0.01):
                    if not select.select([self.fd], [], [], 0)[0]:
                        break
                os.close(self.fd)
                self.closed = True
                return os.waitstatus_to_exitcode(code), self.output.decode()
        self.close()
        raise AssertionError(f'Terminal did not exit: {self.output!r}')

    def close(self):
        if not self.closed:
            import signal
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
            os.close(self.fd)
            self.closed = True


@unittest.skipUnless(os.name == 'posix' and BINARY.exists(), 'requires Unix and a compiled Haps binary')
class ChooserTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.catalog = Catalog(self.temp.name)

    def terminal(self, *args, **kwargs):
        term = Terminal(self.catalog, *args, **kwargs)
        self.addCleanup(term.close)
        return term

    def test_arrow_selection_and_signed_notes(self):
        term = self.terminal('install', 'hello', '--require-attestations', '2', columns=48)
        menu = term.read_until(b'No trusted attestations').decode()
        self.assertLess(menu.index('alice/hello'), menu.index('buildbot/hello'))
        self.assertNotIn('other/hello', menu)
        self.assertIn('Attested by bob, carol', menu)
        self.assertIn('Followed by alice, bob, carol and 1 other you follow', menu)
        self.assertNotIn('hops away', menu)
        term.send(b'\x1b[B\x1b[A\r')
        code, output = term.finish()
        self.assertEqual(code, 0, output)
        self.assertIn('Installed alice/hello 1.0.0', output)
        self.assertIn('bob: Built from source; tests pass.', output)
        self.assertEqual(self.catalog.run('reader', 'run', 'hello').stdout.strip(), 'Hello from Haps')

    def test_mutual_followers_in_json(self):
        info = json.loads(self.catalog.run('reader', 'info', 'buildbot/hello', '--json').stdout)
        self.assertEqual({p['label'] for p in info['followed_by']}, {'alice', 'bob', 'carol', 'dave'})
        self.assertEqual({p['pubkey'] for p in info['followed_by']}, {self.catalog.keys[n] for n in ['alice', 'bob', 'carol', 'dave']})

    def test_cancel_and_extended_graph_choice(self):
        term = self.terminal('install', 'hello')
        term.read_until(b'No trusted attestations')
        term.send(b'\x1b')
        code, output = term.finish()
        self.assertNotEqual(code, 0)
        self.assertIn('cancelled', output)
        self.assertEqual(self.catalog.run('reader', 'list', '--json').stdout.strip(), '[]')
        term = self.terminal('install', 'hello')
        term.read_until(b'No trusted attestations')
        term.send(b'\x1b[B\r')
        code, output = term.finish()
        self.assertNotEqual(code, 0, output)
        self.assertIn('audit(s)', output)
        self.assertEqual(json.loads(self.catalog.run('reader', 'list', '--json').stdout), [])

    def test_agents_never_prompt_even_in_a_terminal(self):
        for flags, env in [(['--non-interactive'], {}), ([], {'HAPS_NON_INTERACTIVE': 'true'}), (['--json'], {})]:
            term = self.terminal('install', 'hello', *flags, env_extra=env)
            code, output = term.finish()
            self.assertNotEqual(code, 0)
            self.assertNotIn('Enter select', output)
            self.assertNotIn('\x1b', output)
            if flags == ['--json']:
                value = json.loads(output)['error']
                self.assertEqual(value['code'], 'ambiguous_package')
                self.assertEqual(value['candidates'][0]['publisher'], self.catalog.keys['alice'])
                self.assertEqual(len(value['candidates'][0]['attestations']), 2)
                self.assertEqual(len(value['candidates']), 2)
        installed = json.loads(self.catalog.run('reader', 'install', f'{self.catalog.keys["alice"]}/hello', '--version', '1.0.0', '--require-attestations', '2', '--json').stdout)
        self.assertEqual(installed['status'], 'installed')
        self.assertEqual(installed['release']['release_id'], self.catalog.releases['alice']['id'])


if __name__ == '__main__':
    unittest.main()
