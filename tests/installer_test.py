import hashlib
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

SCRIPT = Path(__file__).parents[1] / 'website/public/install.sh'

class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='haps installer ')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.stub = self.root / 'tools'
        self.stub.mkdir()
        self.bin = self.root / 'bin with spaces'
        self.bin.mkdir()
        (self.bin / 'haps').write_text('old executable')
        self.env = dict(os.environ, PATH=str(self.stub) + os.pathsep + os.environ['PATH'],
                        HOME=str(self.root), HAPS_INSTALL_DIR=str(self.bin),
                        HAPS_RELEASE_BASE_URL='https://releases.example/haps',
                        FIXTURES=str(self.root), HAPS_TEST_OS='Linux', HAPS_TEST_ARCH='x86_64')
        self.tool('uname', '#!/bin/sh\ncase "$1" in -s) echo "$HAPS_TEST_OS";; -m) echo "$HAPS_TEST_ARCH";; esac\n')
        self.tool('curl', '''#!/usr/bin/env python3
import os,sys,shutil
from pathlib import Path
args=sys.argv[1:]
assert args[args.index('--proto')+1]=='=https'
assert args[args.index('--proto-redir')+1]=='=https'
url=args[args.index('-o')-1]; output=args[args.index('-o')+1]
root=Path(os.environ['FIXTURES'])
with (root/'requests').open('a') as log: log.write(url+'\\n')
source=root/url.rsplit('/',1)[-1]
if not source.exists(): sys.exit(22)
shutil.copyfile(source, output)
''')
        (self.root / 'version.txt').write_text('v0.1.3\n')

    def tool(self, name, content):
        p = self.stub / name
        p.write_text(content)
        p.chmod(0o755)

    def archive(self, target='x86_64-unknown-linux-gnu', version='0.1.3', member='haps', bundled=False, broken_helper=False):
        name = f'haps-v0.1.3-{target}.tar.gz'
        binary = f'#!/bin/sh\necho haps {version}\n'.encode()
        archive = self.root / name
        with tarfile.open(archive, 'w:gz') as bundle:
            info = tarfile.TarInfo(member)
            info.size = len(binary)
            info.mode = 0o755
            bundle.addfile(info, io.BytesIO(binary))
            if bundled:
                for entry, content in {
                    'bundle.json': b'{"hashtree":"v0.2.151"}',
                    'libexec/hashtree-LICENSE': b'MIT fixture license',
                    'libexec/htree': b'#!/bin/sh\necho htree 0.2.151\n',
                    'libexec/git-remote-htree': b'#!/bin/sh\necho "Usage: git-remote-htree" >&2\nexit 1\n' if not broken_helper else b'broken executable',
                }.items():
                    info = tarfile.TarInfo(entry)
                    info.size = len(content)
                    info.mode = 0o755
                    bundle.addfile(info, io.BytesIO(content))
        (self.root / 'SHA256SUMS').write_text(hashlib.sha256(archive.read_bytes()).hexdigest() + '  ' + name + '\n')
        return archive

    def run_installer(self, *args):
        return subprocess.run(['sh', str(SCRIPT), *args], env=self.env, capture_output=True, text=True)

    def test_unix_platforms_and_atomic_reinstallation(self):
        for system, machine, target in [('Linux', 'x86_64', 'x86_64-unknown-linux-gnu'), ('Linux', 'aarch64', 'aarch64-unknown-linux-gnu'), ('Darwin', 'arm64', 'aarch64-apple-darwin'), ('Darwin', 'x86_64', 'x86_64-apple-darwin')]:
            with self.subTest(target=target):
                self.env.update(HAPS_TEST_OS=system, HAPS_TEST_ARCH=machine)
                self.archive(target)
                result = self.run_installer()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(subprocess.check_output([str(self.bin/'haps'), '--version'], text=True).strip(), 'haps 0.1.3')
                self.assertFalse(list(self.bin.glob('.haps.*')))

    def test_bad_download_checksum_version_and_archive_preserve_old_binary(self):
        for failure in ['download', 'checksum', 'version', 'path']:
            with self.subTest(failure=failure):
                archive = self.archive(version='9.0.0' if failure == 'version' else '0.1.3', member='../haps' if failure == 'path' else 'haps')
                if failure == 'download': archive.unlink()
                if failure == 'checksum': archive.write_bytes(b'corrupt')
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual((self.bin/'haps').read_text(), 'old executable')
                self.assertFalse(list(self.bin.glob('.haps.*')))

    def test_pinned_version_and_invalid_input(self):
        self.archive()
        result = self.run_installer('--version', 'v0.1.3')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('version.txt', (self.root/'requests').read_text())
        self.assertNotEqual(self.run_installer('--version', '../evil').returncode, 0)
        self.env['HAPS_TEST_ARCH'] = 'riscv64'
        self.assertNotEqual(self.run_installer().returncode, 0)

    def test_bundle_and_upgrade_preserve_separately_installed_tools(self):
        self.archive(bundled=True)
        (self.bin / 'htree').write_text('separately managed htree')
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        first = (self.bin / 'haps').resolve()
        self.assertTrue((first.parent / 'libexec/git-remote-htree').exists())
        self.assertEqual((self.bin / 'htree').read_text(), 'separately managed htree')
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        second = (self.bin / 'haps').resolve()
        self.assertNotEqual(first, second)
        self.assertTrue(first.exists(), 'previous bundle remains recoverable')
        self.assertEqual((self.bin / 'htree').read_text(), 'separately managed htree')

    def test_broken_bundled_helper_preserves_existing_install(self):
        self.archive(bundled=True, broken_helper=True)
        self.assertNotEqual(self.run_installer().returncode, 0)
        self.assertEqual((self.bin / 'haps').read_text(), 'old executable')

if __name__ == '__main__':
    unittest.main()
