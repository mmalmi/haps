"""Include unchanged, checksum-pinned Hashtree release executables."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parent

def check_release():
    """Fail before building if companions are not publicly downloadable."""
    lock = json.loads((ROOT / 'hashtree-bundle.json').read_text())
    url = f'https://api.github.com/repos/mmalmi/hashtree/releases/tags/{lock["tag"]}'
    request = urllib.request.Request(url, headers={'User-Agent': 'haps-release'})
    with urllib.request.urlopen(request, timeout=30) as response:
        release = json.load(response)
    assert release['tag_name'] == lock['tag']
    assert not release['draft'] and not release['prerelease']
    assets = {asset['name']: asset for asset in release['assets']}
    for asset in lock['assets'].values():
        assert assets[asset['name']]['digest'] == 'sha256:' + asset['sha256']
    print(f'All five pinned Hashtree {lock["tag"]} assets are published')

def stage_helpers(target, destination, cache):
    lock = json.loads((ROOT / 'hashtree-bundle.json').read_text())
    asset = lock['assets'][target]
    cache = cache / lock['tag']
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / asset['name']
    if not archive.exists():
        url = f'https://github.com/mmalmi/hashtree/releases/download/{lock["tag"]}/{asset["name"]}'
        with urllib.request.urlopen(url, timeout=120) as response, archive.open('wb') as output:
            shutil.copyfileobj(response, output)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != asset['sha256']:
        raise ValueError('Hashtree archive checksum mismatch')
    suffix = '.exe' if 'windows' in target else ''
    destination.mkdir(parents=True, exist_ok=True)
    for tool in ['htree', 'git-remote-htree']:
        name = tool + suffix
        member = 'hashtree/' + name
        if archive.suffix == '.zip':
            with zipfile.ZipFile(archive) as bundle:
                info = bundle.getinfo(member)
                if info.is_dir() or (info.external_attr >> 16) & 0o170000 == 0o120000:
                    raise ValueError('Helper must be a regular file')
                data = bundle.read(member)
        else:
            with tarfile.open(archive) as bundle:
                info = bundle.getmember(member)
                if not info.isfile():
                    raise ValueError('Helper must be a regular file')
                data = bundle.extractfile(info).read()
        (destination / name).write_bytes(data)
        (destination / name).chmod(0o755)
    version = subprocess.check_output([str(destination / ('htree' + suffix)), '--version'], text=True).strip()
    assert version == 'htree ' + lock['tag'][1:], version
    helper = subprocess.run([str(destination / ('git-remote-htree' + suffix))], capture_output=True, text=True)
    assert helper.returncode != 0 and 'Usage: git-remote-htree' in helper.stderr
    shutil.copyfile(ROOT / 'hashtree-LICENSE', destination / 'hashtree-LICENSE')
    return {'hashtree': lock['tag'], 'archive_sha256': asset['sha256']}

if __name__ == '__main__':
    check_release()
