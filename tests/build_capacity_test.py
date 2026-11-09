"""Filesystem admission and extraction boundaries; no real tool/network calls."""
import contextlib
import hashlib
import importlib.util
import io
import os
from pathlib import Path
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('capacity_inputs', ROOT / 'scripts/build-inputs.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class BuildCapacity(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.enterContext(contextlib.redirect_stdout(io.StringIO()))
        self.execute = self.enterContext(patch.object(module.subprocess, 'run', side_effect=AssertionError('No process allowed')))
        self.enterContext(patch.object(module.subprocess, 'check_output', side_effect=AssertionError('No compiler allowed')))
        self.fetch = self.enterContext(patch.object(module.urllib.request, 'urlopen', side_effect=AssertionError('No network allowed')))

    def test_exact_entry_and_one_byte_below(self):
        growth = 4 * module.GIB
        needed = 27 * module.GIB
        for free in (needed, needed - 1):
            with self.subTest(free=free), patch.object(module.shutil, 'disk_usage', return_value=SimpleNamespace(free=free)):
                if free == needed:
                    result = module.require_capacity([self.root], growth, 'inputs')
                    self.assertEqual(result['required_free_bytes'], needed)
                else:
                    with self.assertRaisesRegex(ValueError, '20 GiB reserve'):
                        module.require_capacity([self.root], growth, 'inputs')

    def test_same_filesystem_checked_once_and_missing_paths_not_created(self):
        destination = self.root / 'not-created' / 'prefix'
        with patch.object(module.shutil, 'disk_usage', return_value=SimpleNamespace(free=50*module.GIB)) as usage:
            module.require_capacity([destination, self.root], 4*module.GIB, 'inputs')
            usage.assert_called_once_with(self.root)
        self.assertFalse(destination.parent.exists())

    def test_separate_filesystems_never_pool_free_space(self):
        first = self.root / 'first'; second = self.root / 'second'
        first.mkdir(); second.mkdir()
        original_stat = Path.stat
        def device(path, *args, **kwargs):
            values = list(original_stat(path, *args, **kwargs))
            values[2] = 2 if path == second else 1
            return os.stat_result(values)
        def space(path):
            return SimpleNamespace(free=(22 if path == second else 100)*module.GIB)
        with patch.object(Path, 'stat', device), patch.object(module.shutil, 'disk_usage', side_effect=space) as usage:
            with self.assertRaisesRegex(ValueError, 'Insufficient filesystem capacity'):
                module.require_capacity([first, second], 4*module.GIB, 'inputs')
            self.assertEqual(usage.call_count, 2)

    @contextlib.contextmanager
    def windows_layout(self, system_gib):
        system = self.root / 'system'; work = self.root / 'work'
        system.mkdir(); work.mkdir()
        prefix = system / '.rustup'; prefix.mkdir()
        cache = system / '.cargo'; cache.mkdir()
        original_stat = Path.stat
        def device(path, *args, **kwargs):
            values = list(original_stat(path, *args, **kwargs))
            values[2] = 2 if path == work else 1
            return os.stat_result(values)
        def space(path):
            return SimpleNamespace(free=(147 if path == work else system_gib) * module.GIB)
        with patch.object(Path, 'stat', device), patch.object(module.shutil, 'disk_usage', side_effect=space):
            yield prefix, cache, work

    def test_dedicated_toolchain_and_work_drives_keep_full_reserves(self):
        with self.windows_layout(31) as (prefix, cache, work):
            result = module.require_capacity([prefix, work], 12 * module.GIB, 'job', toolchain_root=prefix)
        self.assertEqual([v['required_free_bytes'] for v in result['filesystems']],
                         [27 * module.GIB, 35 * module.GIB])
        self.assertEqual(result['reserve_bytes'], 20 * module.GIB)

    def test_dedicated_toolchain_cannot_borrow_work_drive_reserve(self):
        with self.windows_layout(26) as (prefix, cache, work):
            with self.assertRaisesRegex(ValueError, 'Insufficient filesystem capacity'):
                module.require_capacity([prefix, work], 12 * module.GIB, 'job', toolchain_root=prefix)

    def test_cache_left_on_toolchain_drive_keeps_full_phase_budget(self):
        with self.windows_layout(31) as (prefix, cache, work):
            with self.assertRaisesRegex(ValueError, 'Insufficient filesystem capacity'):
                module.require_capacity([prefix, cache, work], 12 * module.GIB, 'job', toolchain_root=prefix)

    def test_shared_filesystem_keeps_full_budget_in_either_path_order(self):
        prefix = self.root / '.rustup'; prefix.mkdir()
        for paths in ([prefix, self.root], [self.root, prefix]):
            with self.subTest(paths=paths), patch.object(module.shutil, 'disk_usage',
                    return_value=SimpleNamespace(free=31 * module.GIB)) as usage:
                with self.assertRaisesRegex(ValueError, 'Insufficient filesystem capacity'):
                    module.require_capacity(paths, 12 * module.GIB, 'job', toolchain_root=prefix)
                self.assertEqual(usage.call_count, 1)

    def test_unreadable_filesystem_fails_closed(self):
        with patch.object(module.shutil, 'disk_usage', side_effect=OSError('unavailable filesystem')):
            with self.assertRaises(OSError):module.require_capacity([self.root], 0, 'boundary')
        self.fetch.assert_not_called(); self.execute.assert_not_called()

    def test_missing_filesystem_root_does_not_loop(self):
        # Bound the old traversal so a regression fails instead of hanging CI.
        with patch.object(Path, 'resolve', return_value=Path(self.root.anchor)), \
                patch.object(Path, 'stat', side_effect=[FileNotFoundError(2, 'Missing filesystem')] * 3 + [AssertionError('Traversal loop')]):
            with self.assertRaisesRegex(ValueError, 'Cannot locate output filesystem'):
                module.require_capacity([self.root], 0, 'boundary')

    def test_inaccessible_output_path_is_not_replaced_with_parent(self):
        with patch.object(Path, 'resolve', return_value=self.root), \
                patch.object(Path, 'stat', side_effect=PermissionError('Output path inaccessible')):
            with self.assertRaises(PermissionError):
                module.require_capacity([self.root], 0, 'boundary')

    def test_missing_or_invalid_growth_plan_cannot_pass(self):
        for growth in (-1, True, '4'):
            with self.subTest(growth=growth), self.assertRaises(ValueError):
                module.require_capacity([self.root], growth, 'inputs')
        with self.assertRaises(ValueError):module.require_capacity([], 0, 'boundary')

    def test_all_output_roots_and_explicit_docker_root_are_checked(self):
        with patch.dict(os.environ, {'CARGO_HOME': str(self.root/'cargo'), 'CARGO_TARGET_DIR': str(self.root/'target')}), \
                patch.object(module, 'require_capacity') as capacity:
            module.check_capacity(self.root/'inputs', 'build', [self.root/'docker'])
        paths, growth, phase = capacity.call_args.args
        for path in (self.root/'inputs', self.root/'cargo', self.root/'target', self.root/'docker',
                     ROOT, Path.home()/'.rustup', Path(tempfile.gettempdir())):
            self.assertIn(path, paths)
        self.assertEqual(growth, 8*module.GIB); self.assertEqual(phase, 'build')

    def lock(self):
        return {'rust': {'version':'1.99.1', 'archives': {'fixture': {
            'url':'https://example.invalid/rust.tar.xz', 'sha256':hashlib.sha256(b'expected').hexdigest()}}}}

    def install_context(self):
        stack = contextlib.ExitStack()
        stack.enter_context(patch.object(module, 'host_target', return_value='fixture'))
        stack.enter_context(patch.object(Path, 'home', return_value=self.root/'home'))
        return stack

    def test_low_disk_refuses_before_rust_download(self):
        with self.install_context(), patch.object(module, 'check_capacity', side_effect=ValueError('low disk')), \
                patch.object(module, 'download') as download:
            with self.assertRaisesRegex(ValueError, 'low disk'):
                module.install_rust(self.lock(), self.root, bash='unused')
            download.assert_not_called(); self.execute.assert_not_called()

    def test_checksum_failure_precedes_catalog_or_extraction(self):
        self.fetch.side_effect = None; self.fetch.return_value = io.BytesIO(b'corrupt')
        with self.install_context(), patch.object(module, 'check_capacity'), patch.object(module, 'rust_expansion') as catalog:
            with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
                module.install_rust(self.lock(), self.root, bash='unused')
            catalog.assert_not_called(); self.execute.assert_not_called()

    def test_download_byte_cap_never_writes_excess_or_executes(self):
        self.fetch.side_effect = None; self.fetch.return_value = io.BytesIO(b'x'*101)
        spec = {'url':'https://example.invalid/input', 'sha256':hashlib.sha256(b'x'*101).hexdigest()}
        destination = self.root/'input'
        with self.assertRaisesRegex(ValueError, 'download byte allowance'):
            module.download(spec, destination, maximum_bytes=100)
        self.assertFalse(destination.exists()); self.execute.assert_not_called()

    def test_oversized_cached_archive_is_preserved_and_not_executed(self):
        path = self.root/'archive'; path.write_bytes(b'x'*101)
        with self.assertRaisesRegex(ValueError, 'Cached build input'):
            module.download({'url':'https://example.invalid/input','sha256':'0'*64}, path, maximum_bytes=100)
        self.assertEqual(path.read_bytes(), b'x'*101)
        self.fetch.assert_not_called(); self.execute.assert_not_called()

    def archive(self):
        path = self.root/'archive.tar.xz'
        root = 'rust-1.99.1-fixture'
        with tarfile.open(path, 'w:xz') as stream:
            for name, data in [('rustc/bin/rustc',b'x'), ('rust-docs/full.html',b'd'*8193)]:
                item = tarfile.TarInfo(root+'/'+name); item.size = len(data)
                stream.addfile(item,io.BytesIO(data))
            alias = tarfile.TarInfo(root+'/rustc/bin/copy'); alias.type = tarfile.LNKTYPE
            alias.linkname = root+'/rustc/bin/rustc'; stream.addfile(alias)
        return path

    def test_full_unselected_archive_and_installed_hardlink_copy_are_counted(self):
        full, installed = module.rust_expansion(self.archive(), 'rust-1.99.1-fixture', ['rustc'])
        self.assertEqual(installed, 2*8192)
        self.assertEqual(full, installed+16384)

    def test_archive_shape_or_unknown_hardlink_target_is_rejected(self):
        with self.assertRaisesRegex(ValueError, 'Unexpected Rust archive'):
            module.rust_expansion(self.archive(), 'different-root', ['rustc'])
        path = self.root/'link.tar.xz'
        with tarfile.open(path,'w:xz') as stream:
            item = tarfile.TarInfo('rust/root'); item.type = tarfile.LNKTYPE; item.linkname = 'rust/missing'
            stream.addfile(item)
        with self.assertRaisesRegex(ValueError, 'no regular archive target'):
            module.rust_expansion(path, 'rust', [])

    def test_measured_overflow_refuses_before_extraction(self):
        archive = self.archive()
        with self.install_context(), patch.object(module,'download',return_value=archive), \
                patch.object(module,'check_capacity'), patch.object(module,'rust_expansion',return_value=(4*module.GIB,1)):
            with self.assertRaisesRegex(ValueError, 'overlap exceeds'):
                module.install_rust(self.lock(),self.root,bash='unused')
        self.execute.assert_not_called(); self.assertTrue(archive.exists())

    def test_fresh_capacity_loss_refuses_before_extraction(self):
        archive = self.archive()
        with self.install_context(), patch.object(module,'download',return_value=archive), \
                patch.object(module,'check_capacity',side_effect=[None,ValueError('changed capacity')]):
            with self.assertRaisesRegex(ValueError,'changed capacity'):
                module.install_rust(self.lock(),self.root,bash='unused')
        self.execute.assert_not_called()

    def test_fresh_capacity_loss_after_extraction_refuses_installer(self):
        archive = self.archive(); self.execute.side_effect = None
        with self.install_context(), patch.object(module,'download',return_value=archive), \
                patch.object(module,'check_capacity',side_effect=[None,None,ValueError('changed capacity')]):
            with self.assertRaisesRegex(ValueError,'changed capacity'):
                module.install_rust(self.lock(),self.root,bash='unused')
        self.execute.assert_called_once()
        self.assertEqual(self.execute.call_args.args[0][0], 'tar')


if __name__ == '__main__':
    unittest.main()
