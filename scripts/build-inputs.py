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
import subprocess
import tempfile
import urllib.request

LOCK = Path(__file__).with_suffix('.json')


def download(spec, destination):
    """Verify a source-controlled checksum before any extraction or execution."""
    if not re.fullmatch(r'[0-9a-f]{64}', spec['sha256']) or not spec['url'].startswith('https://'):
        raise ValueError('Build input needs an HTTPS URL and fixed SHA-256')
    destination = Path(destination)
    destination.parent.mkdir(parents=True, exist_ok=True)
    if not destination.exists():
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as output:
                temporary = Path(output.name)
                with urllib.request.urlopen(spec['url'], timeout=120) as response:
                    shutil.copyfileobj(response, output)
            verify(temporary, spec['sha256'])
            temporary.replace(destination)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
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
    archive = download(rust['archives'][target], work / ('rust-' + target + '.tar.xz'))
    # Use an existing trusted-tool root so Haps's unchanged isolation policy can
    # expose this exact compiler to source builds without exposing runner scratch.
    prefix = (Path.home() / '.rustup' / 'toolchains' / ('haps-pinned-' + rust['version'] + '-' + target)).resolve()
    if prefix.exists():
        raise ValueError('Pinned toolchain destination already exists; use a fresh CI runner')
    components = ['rustc', 'cargo', 'rust-std-' + target, 'rustfmt-preview', 'clippy-preview']
    with tempfile.TemporaryDirectory(dir=work, prefix='rust-installer-') as temporary:
        subprocess.run(['tar', '-xJf', str(archive), '-C', temporary], check=True)
        installer = Path(temporary) / ('rust-' + rust['version'] + '-' + target) / 'install.sh'
        subprocess.run([bash or 'bash', installer.as_posix(), '--prefix=' + prefix.as_posix(),
                        '--components=' + ','.join(components), '--disable-ldconfig'], check=True)
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


def install_ubuntu_test_tools(lock, work):
    if host_target() != 'x86_64-unknown-linux-gnu':
        raise ValueError('Ubuntu test tools require the x86_64 Ubuntu runner')
    packages = [download(spec, work / spec['url'].rsplit('/', 1)[1]) for spec in lock['ubuntu_test_packages']]
    # dpkg never downloads dependencies. The runner must already provide them.
    subprocess.run(['sudo', 'dpkg', '-i', *map(str, packages)], check=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['rust', 'ubuntu-test-tools'])
    parser.add_argument('--work', type=Path, required=True)
    parser.add_argument('--target')
    parser.add_argument('--bash', help='Installer shell; required for Git Bash on Windows')
    args = parser.parse_args()
    args.work.mkdir(parents=True, exist_ok=True)
    lock = json.loads(LOCK.read_text())
    if args.action == 'rust':
        install_rust(lock, args.work, args.target, args.bash)
    else:
        install_ubuntu_test_tools(lock, args.work)
