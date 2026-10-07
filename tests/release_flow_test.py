import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('release_adapter', ROOT / 'src/integrations/release.py')
adapter = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(adapter)
BINARY = Path(os.environ.get('HAPS_TEST_BINARY', ROOT / 'target/debug/haps'))


def stage(root, tag, content, name='haps', member=None, kind=None):
    root.mkdir(parents=True, exist_ok=True)
    archive = root / f'{name}-{tag}.tar.gz'
    with tarfile.open(archive, 'w:gz') as bundle:
        info = tarfile.TarInfo(member or name)
        info.size = len(content)
        info.mode = 0o755
        if kind:
            info.type = kind
            info.linkname = '/tmp/escape'
        bundle.addfile(info, io.BytesIO(content))
    record = {'tag': tag, 'commit': 'a' * 40, 'draft': False,
              'assets': [{'name': archive.name, 'path': archive.name,
                          'sha256': adapter.digest(archive), 'size': archive.stat().st_size}]}
    (root / 'release.json').write_text(json.dumps(record))
    return record


class ReleaseAdapter(unittest.TestCase):
    def test_rejects_changed_assets_drafts_paths_and_links(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            config = {'source': 'https://example.com/repo', 'packages': [
                {'name': 'haps', 'asset': 'haps-{tag}.tar.gz', 'target': 'test',
                 'description': 'Manager', 'commands': {'haps': 'haps'}}]}
            directory, output = root / 'stage', root / 'out'
            output.mkdir()
            record = stage(directory, 'v1.0.0', b'hello')
            record['draft'] = True
            (directory / 'release.json').write_text(json.dumps(record))
            with self.assertRaisesRegex(ValueError, 'stable'):
                adapter.prepare(directory, config, output, 'v1.0.0')
            record['draft'] = False
            record['assets'][0]['sha256'] = '0' * 64
            (directory / 'release.json').write_text(json.dumps(record))
            with self.assertRaisesRegex(ValueError, 'checksum'):
                adapter.prepare(directory, config, output, 'v1.0.0')
            for index, (member, kind) in enumerate([('../escape', None), ('haps', tarfile.SYMTYPE)]):
                stage(directory, 'v1.0.0', b'bad', member=member, kind=kind)
                current = root / f'bad-{index}'
                current.mkdir()
                with self.assertRaises(ValueError):
                    adapter.prepare(directory, config, current, 'v1.0.0')
            self.assertFalse((root / 'escape').exists())

    def test_date_revisions_preserve_existing_package_version_order(self):
        self.assertEqual(adapter.version('v2026.10.5.2', 'date-revision'), '2026.10.502')
        self.assertEqual(adapter.version('v2026.10.6', 'date-revision'), '2026.10.600')

    @unittest.skipUnless(os.name == 'posix', 'shell launcher lifecycle')
    def test_real_cli_release_retry_install_link_self_update_and_rollback(self):
        self.assertTrue(BINARY.is_file(), 'build haps before running release integration tests')
        with tempfile.TemporaryDirectory(prefix="haps import-release ' $ ") as temp:
            root = Path(temp)
            author, reader = root / 'author', root / 'reader'
            env = dict(os.environ, HAPS_NO_DEFAULTS='true', HTREE_CONFIG_DIR=str(root / 'htree'),
                       HAPS_PYTHON=os.environ.get('HAPS_PYTHON', 'python3'))

            def run(home, *args, success=True, binary=BINARY):
                result = subprocess.run([str(binary), '--home', str(home), *map(str, args)],
                                        env=env, text=True, capture_output=True)
                self.assertEqual(result.returncode == 0, success, result.stderr + result.stdout)
                return result.stdout.strip()

            publisher = run(author, 'identity', 'init')
            target = run(author, 'target')
            config = root / 'haps-release.json'
            config.write_text(json.dumps({'publisher': publisher, 'catalog': 'tools',
                'source': 'https://example.com/tools', 'packages': [
                    {'name': 'haps', 'description': 'Haps manager', 'target': target,
                     'asset': 'haps-{tag}.tar.gz', 'commands': {'haps': 'haps'}}]}))
            release = root / 'release'
            import shlex

            def prepare(version, content=None, success=True):
                content = content or (f'#!/bin/sh\nif [ "$1" = "--version" ]; then echo "haps {version}"; '
                                      f'else exec {shlex.quote(str(BINARY))} "$@"; fi\n').encode()
                stage(release, f'v{version}', content)
                return run(author, 'import-release', release, '--config', config, '--tag', f'v{version}', success=success)

            stage(release, 'v1.0.0', b'#!/bin/sh\necho hello\n')
            run(author, 'import-release', release, '--config', config, '--tag', 'v1.0.0', '--check')
            draft = json.loads((release / 'release.json').read_text())
            draft['draft'] = True
            (release / 'release.json').write_text(json.dumps(draft))
            run(author, 'import-release', release, '--config', config, '--tag', 'v1.0.0', '--check')
            run(author, 'import-release', release, '--config', config, '--tag', 'v1.0.0', success=False)

            self.assertFalse((author / 'catalogs').exists())
            run(author, 'import-release', release, '--config', config, '--tag', 'v9.0.0', '--check', success=False)
            prepare('1.0.0')
            catalog = author / 'catalogs/tools/packages'
            first = (catalog / 'catalog.json').read_bytes()
            prepare('1.0.0')
            self.assertEqual((catalog / 'catalog.json').read_bytes(), first)
            prepare('1.0.0', b'changed release', success=False)
            run(author, 'import-release', release, '--config', config, '--tag', 'v1.0.0', '--check', success=False)
            self.assertEqual((catalog / 'catalog.json').read_bytes(), first)
            run(reader, 'identity', 'init')
            run(reader, 'follow', publisher)
            run(reader, 'source', 'add', 'tools', catalog, '--author', publisher)
            run(reader, 'install', 'haps')
            run(reader, 'link', 'haps')
            launcher = reader / 'bin/haps'
            def linked_version():
                return subprocess.check_output([str(launcher), '--version'], env=env, text=True).strip()
            self.assertEqual(linked_version(), 'haps 1.0.0')
            prepare('1.1.0')
            run(reader, 'update', 'haps', binary=launcher)
            self.assertEqual(linked_version(), 'haps 1.1.0')
            run(reader, 'rollback', 'haps', binary=launcher)
            self.assertEqual(linked_version(), 'haps 1.0.0')
            launcher.write_text('user-owned command')
            run(reader, 'update', 'haps', success=False)
            self.assertEqual(launcher.read_text(), 'user-owned command')
            self.assertIn('1.0.0', run(reader, 'list', '--json'))


if __name__ == '__main__':
    unittest.main()
