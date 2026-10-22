"""Real OS containment: no mocked sandbox and no automatic host fallback."""
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import threading
import unittest

import audit_test


@unittest.skipUnless(sys.platform in ('linux', 'darwin'), 'native build backend unavailable')
class BuildIsolation(unittest.TestCase):
    def setUp(self):
        self.fixture = f = audit_test.Audits('runTest')
        f.setUp()
        self.addCleanup(f.doCleanups)
        f.allow_host_build = False
        self.secret = f.root / 'host-secret'
        self.secret.write_text('host credential canary')
        self.outside = f.root / 'outside-write'
        f.env.update(HAPS_BUILD_SECRET='must-not-leak', SSH_AUTH_SOCK=str(f.root / 'agent.sock'),
                     DOCKER_HOST='unix://' + str(f.root / 'docker.sock'),
                     NODE_OPTIONS='--require=' + str(self.secret))

    def build(self, script, *options, success=True, extra_recipe=''):
        f = self.fixture
        (f.source / 'build.py').write_text(script)
        (f.source / 'haps.toml').write_text('''name="isolated"
version="1.0.0"
target="host"
description="Build isolation fixture"
commands={isolated="output"}
[build]
commands=[["python3","build.py"]]
artifacts={output="output"}
''' + extra_recipe)
        for args in [('init',), ('config', 'user.name', 'Test'),
                     ('config', 'user.email', 'test@example.invalid'), ('add', '.'),
                     ('commit', '-m', 'Isolated build fixture')]:
            subprocess.run(['git', '-C', str(f.source), *args], check=True, capture_output=True)
        rev = subprocess.check_output(['git', '-C', str(f.source), 'rev-parse', 'HEAD'], text=True).strip()
        return f.run_cli('build', f.source.as_uri(), '--rev', rev, '--execute', *options, success=success)

    def test_host_reads_writes_symlinks_environment_and_descendants_are_confined(self):
        f = self.fixture
        (f.source / 'escape').symlink_to(self.secret)
        self.build(f'''import os, pathlib, shutil, subprocess, sys
for key in ('HAPS_BUILD_SECRET','SSH_AUTH_SOCK','DOCKER_HOST','NODE_OPTIONS'):
    assert key not in os.environ, key
assert pathlib.Path.home() != pathlib.Path({str(Path.home())!r})
try: os.rename(os.getcwd(), pathlib.Path(os.environ['TMPDIR'], 'moved-build-root'))
except OSError: pass
else: raise AssertionError('replaced the directory used by the next build policy')
for name in ({str(self.secret)!r}, 'escape', {str(f.home / 'identity.key')!r}, {'/System/Volumes/Data' + str(self.secret.resolve())!r}):
    try: pathlib.Path(name).read_bytes()
    except OSError: pass
    else: raise AssertionError('host read allowed: ' + name)
try: os.link({str(self.secret)!r}, 'hardlink-escape')
except OSError: pass
else: raise AssertionError('linked a host file into the writable build root')
for name in ({str(self.outside)!r}, {str(self.secret)!r}):
    try: pathlib.Path(name).write_text('changed')
    except OSError: pass
    else: raise AssertionError('host write allowed: ' + name)
child = subprocess.run(['sh','-c','printf child > "$1"','sh',{str(self.outside)!r}])
assert child.returncode != 0
if shutil.which('sudo'):
    try: sudo = subprocess.run(['sudo','-n','touch',{str(self.outside)!r}], stdin=subprocess.DEVNULL, capture_output=True)
    except OSError: pass
    else: assert sudo.returncode != 0
if sys.platform == 'linux':
    status = pathlib.Path('/proc/self/status').read_text()
    assert 'NoNewPrivs:\\t1' in status
    assert 'CapEff:\\t0000000000000000' in status
if sys.platform == 'darwin':
    import ctypes
    libc = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)
    mib = (ctypes.c_int * 3)(1, 49, {os.getpid()})  # CTL_KERN, KERN_PROCARGS2, host process
    size = ctypes.c_size_t(0)
    if libc.sysctl(mib, 3, None, ctypes.byref(size), None, 0) == 0:
        assert 0 < size.value <= 16 * 1024 * 1024
        buffer = ctypes.create_string_buffer(size.value)
        assert libc.sysctl(mib, 3, buffer, ctypes.byref(size), None, 0) != 0
pathlib.Path(os.environ['TMPDIR'], 'temporary').write_text('allowed')
pathlib.Path('output').write_text('isolated artifact')
''')
        self.assertEqual(self.secret.read_text(), 'host credential canary')
        self.assertFalse(self.outside.exists())

    def test_host_control_socket_is_unavailable_even_when_network_is_allowed(self):
        address = self.fixture.root / 'control.sock'
        server = socket.socket(socket.AF_UNIX)
        self.addCleanup(server.close)
        server.bind(str(address))
        server.listen()
        (self.fixture.source / 'socket-link').symlink_to(address)
        self.build(f'''import socket, pathlib
for path in ({str(address)!r}, 'socket-link'):
    client=socket.socket(socket.AF_UNIX)
    try: client.connect(path)
    except OSError: pass
    else: raise AssertionError('host control socket allowed')
    finally: client.close()
pathlib.Path('output').write_text('isolated artifact')
''', '--allow-build-network')

    def test_network_is_denied_by_default(self):
        server = socket.socket()
        self.addCleanup(server.close)
        server.bind(('127.0.0.1', 0))
        server.listen()
        self.build(f'''import socket, pathlib
try: socket.create_connection(('127.0.0.1', {server.getsockname()[1]}), timeout=1)
except OSError: pass
else: raise AssertionError('network unexpectedly allowed')
pathlib.Path('output').write_text('offline artifact')
''')

    def test_explicit_network_permission_retains_file_and_environment_protection(self):
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'build dependency')
            def log_message(self, *args): pass
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        def stop():
            server.shutdown()
            server.server_close()
            thread.join(timeout=3)
        self.addCleanup(stop)
        result = self.build(f'''import hashlib, os, pathlib, urllib.request
assert 'HAPS_BUILD_SECRET' not in os.environ
content = urllib.request.urlopen('http://127.0.0.1:{server.server_port}/dependency', timeout=3).read()
assert hashlib.sha256(content).hexdigest() == 'f2dd40102567794147053f0242d0e134aab1ea02f6c5ac9fe552d2094e331ddb'
try: pathlib.Path({str(self.secret)!r}).read_bytes()
except OSError: pass
else: raise AssertionError('network permission exposed host files')
pathlib.Path('output').write_bytes(content)
''', '--allow-build-network')
        self.assertIn('including host/local-network services', result.stderr)

    def test_recipe_cannot_grant_permissions(self):
        result = self.build("raise AssertionError('must not execute')", success=False,
                            extra_recipe='allow_host = true\n')
        self.assertIn('unknown field', result.stderr)

    def test_failed_build_never_retries_with_host_permissions(self):
        self.fixture.env['HAPS_ALLOW_HOST_BUILD'] = 'true'
        result = self.build(f'''import pathlib
pathlib.Path({str(self.outside)!r}).write_text('must stay blocked')
pathlib.Path('output').write_text('must not succeed')
''', success=False)
        self.assertFalse(self.outside.exists())
        self.assertIn('No broader permissions were granted', result.stderr)

    def test_child_cannot_signal_a_host_process(self):
        target = subprocess.Popen(['sleep', '30'])
        def stop():
            target.terminate()
            target.wait(timeout=5)
        self.addCleanup(stop)
        self.build(f'''import os, pathlib, signal
try: os.kill({target.pid}, signal.SIGTERM)
except OSError: pass
else: raise AssertionError('signalled a host process')
pathlib.Path('output').write_text('isolated artifact')
''')
        self.assertIsNone(target.poll())

    @unittest.skipUnless(os.environ.get('HAPS_LIVE_BUILD_NETWORK') == '1',
                         'opt-in real registry dependency build')
    def test_locked_cargo_dependency_build_with_explicit_network(self):
        source = self.fixture.source
        (source / 'Cargo.toml').write_text('''[package]
name="locked-builder"
version="0.1.0"
edition="2021"
[dependencies]
itoa="=1.0.18"
''')
        (source / 'Cargo.lock').write_text('''version = 4
[[package]]
name = "locked-builder"
version = "0.1.0"
dependencies = ["itoa"]
[[package]]
name = "itoa"
version = "1.0.18"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "8f42a60cbdf9a97f5d2305f08a87dc4e09308d1276d28c869c684d7777685682"
''')
        (source / 'src').mkdir()
        (source / 'src/main.rs').write_text('fn main() { println!("{}", itoa::Buffer::new().format(42)); }')
        self.build('''import pathlib, subprocess
subprocess.run(['cargo','build','--locked','--jobs','2'], check=True)
output = subprocess.check_output(['target/debug/locked-builder'])
assert output.strip() == b'42'
pathlib.Path('output').write_bytes(output)
''', '--allow-build-network')

    def test_explicit_host_build_is_visible_and_does_not_claim_isolation(self):
        result = self.build(f'''import os, pathlib
assert os.environ['HAPS_BUILD_SECRET'] == 'must-not-leak'
pathlib.Path({str(self.outside)!r}).write_text('explicit host permission')
pathlib.Path('output').write_text('host artifact')
''', '--allow-host-build')
        self.assertTrue(self.outside.exists())
        self.assertIn('no execution isolation', result.stderr)


if __name__ == '__main__':
    unittest.main()
