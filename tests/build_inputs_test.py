import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('build_inputs', ROOT / 'scripts/build-inputs.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class BuildInputs(unittest.TestCase):
    def test_corrupt_download_never_reaches_extraction(self):
        lock = {'rust': {'version': '1.99.1', 'archives': {'fixture': {
            'url': 'https://example.invalid/rust.tar.xz',
            'sha256': hashlib.sha256(b'expected compiler').hexdigest(),
        }}}}
        with tempfile.TemporaryDirectory() as temporary, \
                patch.object(module, 'host_target', return_value='fixture'), \
                patch.object(module.urllib.request, 'urlopen', return_value=io.BytesIO(b'corrupt')), \
                patch.object(module.subprocess, 'run') as execute:
            with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
                module.install_rust(lock, Path(temporary), bash='unused-bash')
            execute.assert_not_called()
            self.assertEqual(list(Path(temporary).iterdir()), [])

    def test_windows_requires_git_bash_before_downloading(self):
        with patch.object(module, 'platform') as platform, \
                patch.object(module, 'host_target', return_value='x86_64-pc-windows-msvc'), \
                patch.object(module, 'download') as fetch:
            platform.system.return_value = 'Windows'
            with self.assertRaisesRegex(ValueError, 'Git Bash'):
                module.install_rust({}, Path('unused'))
            fetch.assert_not_called()

    def test_cached_inputs_are_verified_before_reuse(self):
        data = b'reviewed input'
        spec = {'url': 'https://example.invalid/input', 'sha256': hashlib.sha256(data).hexdigest()}
        with tempfile.TemporaryDirectory() as temporary, \
                patch.object(module.urllib.request, 'urlopen', return_value=io.BytesIO(data)) as fetch:
            destination = Path(temporary) / 'input'
            self.assertEqual(module.download(spec, destination).read_bytes(), data)
            module.download(spec, destination)
            self.assertEqual(fetch.call_count, 1)
            destination.write_bytes(b'tampered cache')
            with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
                module.download(spec, destination)

    def test_corrupt_os_package_cannot_be_installed(self):
        lock = {'ubuntu_test_packages': [{'url': 'https://example.invalid/tool.deb', 'sha256': '0' * 64}]}
        with tempfile.TemporaryDirectory() as temporary, \
                patch.object(module, 'host_target', return_value='x86_64-unknown-linux-gnu'), \
                patch.object(module.urllib.request, 'urlopen', return_value=io.BytesIO(b'corrupt')), \
                patch.object(module.subprocess, 'run') as execute:
            with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
                module.install_ubuntu_test_tools(lock, Path(temporary))
            execute.assert_not_called()

    def test_arch_package_databases_cannot_refresh_after_verification(self):
        script = (ROOT / 'scripts/test-makepkg.sh').read_text()
        lock = json.loads(module.LOCK.read_text())
        self.assertRegex(lock['arch']['image'], r'^archlinux@sha256:[0-9a-f]{64}$')
        self.assertLess(script.index('sha256sum --check --strict'), script.index('pacman -Su'))
        self.assertNotIn('pacman -Syu', script)
        for checksum in lock['arch']['databases'].values():
            self.assertRegex(checksum, r'^[0-9a-f]{64}$')


if __name__ == '__main__':
    unittest.main()
