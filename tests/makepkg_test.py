"""Exercise an actual PKGBUILD build where Arch build tools are installed."""
import hashlib
import os
import shutil
import subprocess
import sys
import unittest

import audit_test


@unittest.skipUnless(sys.platform == 'linux' and shutil.which('makepkg') and os.getuid() != 0,
                     'requires an unprivileged Arch build environment')
class Makepkg(unittest.TestCase):
    def test_reviewed_pkgbuild_builds_installs_and_runs_without_publisher_binary(self):
        f = audit_test.Audits('runTest')
        f.setUp()
        f.allow_host_build = False
        self.addCleanup(f.doCleanups)
        source = '#include <stdio.h>\nint main(void) { puts("Built with makepkg"); return 0; }\n'
        (f.source / 'hello.c').write_text(source)
        digest = hashlib.sha256(source.encode()).hexdigest()
        (f.source / 'PKGBUILD').write_text(f'''pkgname=hello
pkgver=1.0.0
pkgrel=1
arch=('x86_64' 'aarch64')
source=('hello.c')
sha256sums=('{digest}')
build() {{ cc hello.c -o hello; }}
package() {{ install -Dm755 "$srcdir/hello" "$pkgdir/usr/bin/hello"; }}
''')
        (f.source / 'haps.toml').write_text('''# https://haps.hashtree.cc
[package]
name = "hello"
version = "1.0.0"
target = "host"
description = "Arch build fixture"
commands = { hello = "usr/bin/hello" }
[build]
builder = "makepkg"
artifacts = { "usr/bin/hello" = "pkg/hello/usr/bin/hello" }
''')
        def git(*args):
            return subprocess.check_output(['git', '-C', str(f.source), *args], stderr=subprocess.DEVNULL, text=True).strip()
        git('init')
        git('config', 'user.name', 'Fixture')
        git('config', 'user.email', 'fixture@example.invalid')
        git('add', '.')
        git('commit', '-m', 'Pinned PKGBUILD fixture')
        f.run_cli('build', f.source.as_uri(), '--rev', git('rev-parse', 'HEAD'),
                  '--execute', '--install', '--audit-agent', 'codex')
        self.assertEqual(f.run_cli('run', 'hello').stdout.strip(), 'Built with makepkg')
        self.assertTrue((f.root / 'agent-called').exists())


if __name__ == '__main__':
    unittest.main()
