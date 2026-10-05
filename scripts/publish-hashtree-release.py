#!/usr/bin/env python3
"""Verify and mirror completed GitHub binary assets to the maintainer's Hashtree."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

from release import TARGETS

parser = argparse.ArgumentParser()
parser.add_argument('--tag', required=True)
args = parser.parse_args()
tag = args.tag
if not re.fullmatch(r'v\d+\.\d+\.\d+', tag):
    parser.error('expected a stable vX.Y.Z tag')
commit = subprocess.check_output(['git', 'rev-parse', f'{tag}^{{commit}}'], text=True).strip()
with tempfile.TemporaryDirectory(prefix='haps-release-') as temp:
    root = Path(temp)
    assets = root / 'assets'
    assets.mkdir()
    subprocess.run(['gh', 'release', 'download', tag, '--repo', 'mmalmi/haps', '--dir', str(assets)], check=True)
    manifest = json.loads((assets / 'release.json').read_text())
    if manifest['tag'] != tag or manifest['commit'] != commit or manifest['version'] != tag[1:]:
        raise SystemExit('Release manifest does not match the local tag')
    expected_names = {f'haps-{tag}-{t}{".zip" if "windows" in t else ".tar.gz"}' for t in TARGETS}
    if {a['name'] for a in manifest['assets']} != expected_names or len(manifest['assets']) != len(TARGETS):
        raise SystemExit('Release is missing a platform or contains unexpected archives')
    checksums = ''
    for asset in manifest['assets']:
        data = (assets / asset['name']).read_bytes()
        if len(data) != asset['size'] or hashlib.sha256(data).hexdigest() != asset['sha256']:
            raise SystemExit(f'Invalid release asset: {asset["name"]}')
        checksums += f'{asset["sha256"]}  {asset["name"]}\n'
    if (assets / 'SHA256SUMS').read_text() != checksums:
        raise SystemExit('Checksums disagree with release manifest')
    tagged_installer = subprocess.check_output(['git', 'show', f'{tag}:website/public/install.sh'])
    if (assets / 'install.sh').read_bytes() != tagged_installer:
        raise SystemExit('Installer differs from tagged source')
    if {p.name for p in assets.iterdir()} != expected_names | {'release.json', 'SHA256SUMS', 'install.sh'}:
        raise SystemExit('Unexpected release files')
    shutil.copyfile(assets / 'release.json', root / 'release.json')
    (root / 'version.txt').write_text(tag + '\n')
    output = subprocess.check_output(['htree', 'add', str(root), '--no-ignore'], text=True)
    match = re.search(r'^\s*url:\s*(nhash1\w+)\s*$', output, re.M)
    if not match:
        raise SystemExit('htree did not return a release directory hash')
    print('Verified all five platform archives; publishing the same bytes to Hashtree.', flush=True)
    subprocess.run(['htree', 'release', 'publish', 'releases/haps', tag, match[1]], check=True)
