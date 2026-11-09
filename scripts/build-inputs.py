#!/usr/bin/env python3
"""Install reviewed CI inputs; source-build recipes use preinstalled tools."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import urllib.error
import urllib.request

LOCK = Path(__file__).with_suffix('.json')
GIB = 1024 ** 3
DISK_RESERVE = 20 * GIB
# Planning allowances, not quotas or a continuous free-space guarantee. Check
# again at each phase; never reclaim runner data to make an admission pass.
BACKGROUND_GROWTH = 2 * GIB
RESPONSE_MARGIN = GIB
PHASE_GROWTH = {'job': 12 * GIB, 'inputs': 4 * GIB, 'build': 8 * GIB, 'boundary': 0}
# The job allowance covers up to 4 GiB of input staging plus 8 GiB for Cargo
# caches/targets, packaging, isolated test builds and the pinned Arch image.


def require_capacity(paths, planned_bytes, phase, toolchain_root=None):
    if type(planned_bytes) is not int or planned_bytes < 0:
        raise ValueError('Invalid disk growth plan')
    required = DISK_RESERVE + planned_bytes + BACKGROUND_GROWTH + RESPONSE_MARGIN
    filesystems = {}
    for path in paths:
        # The fixed compiler prefix can grow only within the authenticated input
        # plan. Other outputs retain the full phase allowance. On a shared
        # filesystem the larger allowance wins; free space is never pooled.
        growth = min(planned_bytes, PHASE_GROWTH['inputs']) if path == toolchain_root else planned_bytes
        probe = Path(path).resolve()
        while True:
            try:
                info = probe.stat()
                break
            except FileNotFoundError:
                parent = probe.parent
                if parent == probe:
                    raise ValueError('Cannot locate output filesystem') from None
                probe = parent
        if not stat.S_ISDIR(info.st_mode):
            probe = probe.parent
            info = probe.stat()
        device = info.st_dev
        if device not in filesystems:
            filesystems[device] = {'path': str(probe), 'free_bytes': shutil.disk_usage(probe).free,
                                   'planned_growth_bytes': growth}
        else:
            filesystems[device]['planned_growth_bytes'] = max(filesystems[device]['planned_growth_bytes'], growth)
    if not filesystems:
        raise ValueError('No output filesystems specified')
    for volume in filesystems.values():
        volume['required_free_bytes'] = (DISK_RESERVE + volume['planned_growth_bytes']
                                         + BACKGROUND_GROWTH + RESPONSE_MARGIN)
    report = {'phase': phase, 'required_free_bytes': required, 'reserve_bytes': DISK_RESERVE,
              'planned_growth_bytes': planned_bytes, 'background_bytes': BACKGROUND_GROWTH,
              'response_bytes': RESPONSE_MARGIN, 'filesystems': list(filesystems.values())}
    print(json.dumps(report), flush=True)
    if any(volume['free_bytes'] < volume['required_free_bytes'] for volume in filesystems.values()):
        raise ValueError(f'Insufficient filesystem capacity before {phase}; '
                         'each filesystem needs its planned growth plus the 20 GiB reserve, '
                         'background and response allowances. No cleanup attempted.')
    return report


def capacity_paths(work):
    root = Path(__file__).resolve().parents[1]
    return [work, root, Path.home() / '.rustup',
            Path(os.environ.get('CARGO_HOME', str(Path.home() / '.cargo'))),
            Path(os.environ.get('CARGO_TARGET_DIR', str(root / 'target'))),
            Path(tempfile.gettempdir())]


def check_capacity(work, phase, extra_paths=(), planned_bytes=None):
    return require_capacity([*capacity_paths(work), *extra_paths],
                            PHASE_GROWTH[phase] if planned_bytes is None else planned_bytes, phase,
                            toolchain_root=Path.home() / '.rustup')


def rust_expansion(archive, root, components):
    """Plan full extraction plus prefix from authenticated member sizes.

    Four-KiB rounding and per-entry metadata are allowances, not an assertion
    about the filesystem's physical allocation. Fresh phase checks still apply.
    """
    full = installed = 0
    files, hardlinks = {}, []
    with tarfile.open(archive, mode='r|xz') as stream:
        for member in stream:
            parts = member.name.rstrip('/').split('/')
            if not parts or parts[0] != root or '..' in parts:
                raise ValueError('Unexpected Rust archive member')
            selected = len(parts) > 1 and parts[1] in components
            charge = ((member.size + 4095) // 4096) * 4096 + 4096
            full += charge
            if selected:
                installed += charge
            if member.isfile():
                files[member.name] = member.size
            if member.islnk():
                hardlinks.append((member.linkname, selected))
    for target, selected in hardlinks:
        if target not in files:
            raise ValueError('Rust hardlink has no regular archive target')
        # Conservatively allow the installer/extractor to copy hardlinked files.
        charge = ((files[target] + 4095) // 4096) * 4096
        full += charge
        if selected:
            installed += charge
    return full, installed


def download(spec, destination, maximum_bytes=None):
    """Verify a source-controlled checksum before any extraction or execution."""
    mirror = spec.get('mirror')
    if (not re.fullmatch(r'[0-9a-f]{64}', spec['sha256']) or not spec['url'].startswith('https://')
            or (mirror is not None and not mirror.startswith('https://'))):
        raise ValueError('Build input needs an HTTPS URL and fixed SHA-256')
    destination = Path(destination)
    destination.parent.mkdir(parents=True, exist_ok=True)
    if not destination.exists():
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as output:
                temporary = Path(output.name)
                try:
                    response = urllib.request.urlopen(spec['url'], timeout=120)
                except (urllib.error.URLError, TimeoutError):
                    if mirror is None:
                        raise
                    print(f'Primary input unavailable; trying {mirror}', flush=True)
                    response = urllib.request.urlopen(mirror, timeout=120)
                with response:
                    total = 0
                    while block := response.read(1024 * 1024):
                        total += len(block)
                        if maximum_bytes is not None and total > maximum_bytes:
                            raise ValueError('Build input exceeds its download byte allowance')
                        output.write(block)
            verify(temporary, spec['sha256'])
            temporary.replace(destination)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
    if maximum_bytes is not None and destination.stat().st_size > maximum_bytes:
        raise ValueError('Cached build input exceeds its download byte allowance')
    verify(destination, spec['sha256'])
    return destination


def verify(path, expected):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    if digest.hexdigest() != expected:
        raise ValueError('Build input checksum mismatch: ' + Path(path).name)


def host_target():
    arch = 'aarch64' if platform.machine().lower() in ('arm64', 'aarch64') else 'x86_64'
    system = {'Linux': 'unknown-linux-gnu', 'Darwin': 'apple-darwin', 'Windows': 'pc-windows-msvc'}[platform.system()]
    return arch + '-' + system


def prepare_rust_helpers(prefix, target):
    if not target.endswith('-apple-darwin'):
        return
    # The official rustc component puts LLVM in prefix/lib, but rust-objcopy's
    # LC_RPATH is @loader_path/../lib. Direct compiler use (including isolation)
    # must work without rustup's inherited dynamic-library environment.
    library = prefix / 'lib/libLLVM.dylib'
    destination = prefix / 'lib/rustlib' / target / 'lib/libLLVM.dylib'
    if not library.is_file():
        raise ValueError('Pinned compiler is missing its bundled LLVM library')
    if destination.exists() or destination.is_symlink():
        if not destination.samefile(library):
            raise ValueError('Compiler helper library path already contains another input')
    else:
        destination.symlink_to('../../../libLLVM.dylib')
    environment = {key: value for key, value in os.environ.items()
                   if key not in ('DYLD_LIBRARY_PATH', 'DYLD_FALLBACK_LIBRARY_PATH')}
    subprocess.run([str(prefix / 'lib/rustlib' / target / 'bin/rust-objcopy'), '--version'],
                   check=True, stdout=subprocess.PIPE, env=environment)


def install_rust(lock, work, target=None, bash=None):
    target = target or host_target()
    if target != host_target():
        raise ValueError('Use the native runner for the selected target')
    if platform.system() == 'Windows' and bash is None:
        raise ValueError('Pass the installed Git Bash executable with --bash')
    rust = lock['rust']
    # Use an existing trusted-tool root so Haps's unchanged isolation policy can
    # expose this exact compiler to source builds without exposing runner scratch.
    prefix = (Path.home() / '.rustup' / 'toolchains' / ('haps-pinned-' + rust['version'] + '-' + target)).resolve()
    if prefix.exists():
        raise ValueError('Pinned toolchain destination already exists; use a fresh CI runner')
    components = ['rustc', 'cargo', 'rust-std-' + target, 'rustfmt-preview', 'clippy-preview']
    check_capacity(work, 'inputs', [prefix])
    archive = download(rust['archives'][target], work / ('rust-' + target + '.tar.xz'), maximum_bytes=GIB)
    full, installed = rust_expansion(archive, 'rust-' + rust['version'] + '-' + target, components)
    overhead = 32 * 1024 ** 2  # installer manifests, helper lookup, logs
    peak = archive.stat().st_size + full + installed + overhead
    if peak > PHASE_GROWTH['inputs']:
        raise ValueError('Measured Rust archive/extraction/prefix overlap exceeds the 4 GiB plan')
    check_capacity(work, 'inputs', [prefix], planned_bytes=full + installed + overhead)
    with tempfile.TemporaryDirectory(dir=work, prefix='rust-installer-') as temporary:
        subprocess.run(['tar', '-xJf', str(archive), '-C', temporary], check=True)
        check_capacity(work, 'inputs', [prefix], planned_bytes=installed + overhead)
        installer = Path(temporary) / ('rust-' + rust['version'] + '-' + target) / 'install.sh'
        subprocess.run([bash or 'bash', installer.as_posix(), '--prefix=' + prefix.as_posix(),
                        '--components=' + ','.join(components), '--disable-ldconfig'], check=True)
    check_capacity(work, 'boundary', [prefix])
    suffix = '.exe' if platform.system() == 'Windows' else ''
    for name in ('rustc', 'cargo'):
        output = subprocess.check_output([str(prefix / 'bin' / (name + suffix)), '--version'], text=True)
        if not output.startswith(name + ' ' + rust['version'] + ' '):
            raise ValueError('Installed toolchain version mismatch: ' + output)
        print(output.strip())
    prepare_rust_helpers(prefix, target)
    if 'GITHUB_PATH' in os.environ:
        with open(os.environ['GITHUB_PATH'], 'a') as output:
            output.write(str(prefix / 'bin') + '\n')
        with open(os.environ['GITHUB_ENV'], 'a') as output:
            output.write('RUSTC=' + str(prefix / 'bin' / ('rustc' + suffix)) + '\n')
            output.write('RUSTDOC=' + str(prefix / 'bin' / ('rustdoc' + suffix)) + '\n')


def install_ubuntu_test_tools(lock, work):
    if host_target() != 'x86_64-unknown-linux-gnu':
        raise ValueError('Ubuntu test tools require the x86_64 Ubuntu runner')
    check_capacity(work, 'inputs', [Path('/usr'), Path('/var')])
    packages = [download(spec, work / spec['url'].rsplit('/', 1)[1]) for spec in lock['ubuntu_test_packages']]
    # dpkg never downloads dependencies. The runner must already provide them.
    subprocess.run(['sudo', 'dpkg', '-i', *map(str, packages)], check=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['rust', 'ubuntu-test-tools', 'check-capacity'])
    parser.add_argument('--work', type=Path, required=True)
    parser.add_argument('--target')
    parser.add_argument('--bash', help='Installer shell; required for Git Bash on Windows')
    parser.add_argument('--phase', choices=PHASE_GROWTH, default='build')
    parser.add_argument('--path', type=Path, action='append', default=[], help='Additional output filesystem')
    args = parser.parse_args()
    if args.action == 'check-capacity':
        check_capacity(args.work, args.phase, args.path)
        raise SystemExit(0)
    check_capacity(args.work, 'inputs')
    args.work.mkdir(parents=True, exist_ok=True)
    lock = json.loads(LOCK.read_text())
    if args.action == 'rust':
        install_rust(lock, args.work, args.target, args.bash)
    else:
        install_ubuntu_test_tools(lock, args.work)
