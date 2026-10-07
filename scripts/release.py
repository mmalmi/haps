#!/usr/bin/env python3
"""Package, smoke-test, and index tagged Haps binaries without rebuilding them."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import zipfile
from bundle import stage_helpers

TARGETS = [
    'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu',
    'x86_64-apple-darwin', 'aarch64-apple-darwin', 'x86_64-pc-windows-msvc',
]

def run(binary, home, *args):
    return subprocess.check_output([str(binary), '--home', str(home), '--no-defaults', *args], text=True).strip()

def package(tag, target, output):
    assert target in TARGETS
    version = tag.removeprefix('v')
    name = 'haps.exe' if 'windows' in target else 'haps'
    binary = (Path('target') / target / 'release' / name).resolve()
    assert subprocess.check_output([str(binary), '--version'], text=True).strip() == f'haps {version}'
    assert subprocess.check_output([str(binary), 'target'], text=True).strip() == target
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f'haps-{tag}-{target}{".zip" if "windows" in target else ".tar.gz"}'
    with tempfile.TemporaryDirectory() as temp:
        stage = Path(temp)
        shutil.copyfile(binary, stage / name)
        (stage / name).chmod(0o755)
        metadata = stage_helpers(target, stage / 'libexec', Path('work/hashtree-bundle'))
        (stage / 'bundle.json').write_text(json.dumps(metadata, indent=2) + '\n')
        files = sorted(p for p in stage.rglob('*') if p.is_file())
        if 'windows' in target:
            with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as bundle:
                for path in files:
                    bundle.write(path, path.relative_to(stage).as_posix())
        else:
            with tarfile.open(archive, 'w:gz') as bundle:
                for path in files:
                    bundle.add(path, arcname=path.relative_to(stage).as_posix())
    # Exercise the archive's bytes through the production signed package flow.
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        payload = root / 'payload'
        payload.mkdir()
        if 'windows' in target:
            with zipfile.ZipFile(archive) as bundle:
                bundle.extractall(payload)
        else:
            with tarfile.open(archive) as bundle:
                bundle.extractall(payload, filter='data')
        extracted = payload / name
        suffix = '.exe' if 'windows' in target else ''
        subprocess.run([str(payload / 'libexec' / ('htree' + suffix)), '--version'], check=True)
        home = root / 'home'
        author = run(extracted, home, 'identity', 'init')
        spec = root / 'haps.toml'
        spec.write_text(f'name="haps-smoke"\nversion="{version}"\ntarget="{target}"\ndescription="Release smoke test"\n[commands]\nhaps="{name}"\n')
        repository = root / 'repository'
        run(extracted, home, 'pack', str(spec), '--payload', str(payload), '--out', str(repository))
        run(extracted, home, 'source', 'add', 'smoke', str(repository), '--author', author)
        run(extracted, home, 'install', 'haps-smoke')
        assert run(extracted, home, 'run', 'haps-smoke', '--', '--version') == f'haps {version}'
    print(f'Packaged and installed {archive.name}')

def index(tag, commit, output):
    assert re.fullmatch('[0-9a-f]{40}', commit)
    assets = []
    for target in TARGETS:
        name = f'haps-{tag}-{target}{".zip" if "windows" in target else ".tar.gz"}'
        file = output / name
        assets.append({'name': name, 'path': f'assets/{name}', 'target': target, 'size': file.stat().st_size,
                       'sha256': hashlib.sha256(file.read_bytes()).hexdigest()})
    (output / 'SHA256SUMS').write_text(''.join(f'{a["sha256"]}  {a["name"]}\n' for a in assets))
    (output / 'release.json').write_text(json.dumps({'schema': 'haps.binary-release.v1', 'tag': tag, 'version': tag[1:], 'commit': commit, 'companions': json.loads(Path('scripts/hashtree-bundle.json').read_text()), 'assets': assets}, indent=2) + '\n')
    shutil.copyfile('website/public/install.sh', output / 'install.sh')

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=['package', 'index'])
    parser.add_argument('--tag', required=True)
    parser.add_argument('--target', choices=TARGETS)
    parser.add_argument('--commit')
    parser.add_argument('--output', type=Path, default=Path('dist'))
    args = parser.parse_args()
    assert re.fullmatch(r'v\d+\.\d+\.\d+', args.tag), 'invalid release tag'
    if args.action == 'package':
        package(args.tag, args.target, args.output)
    else:
        index(args.tag, args.commit, args.output)
