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

def stage_helpers(target, destination, cache):
    lock = json.loads((ROOT / 'hashtree-bundle.json').read_text())
    asset = lock['assets'][target]
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
