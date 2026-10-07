"""Read the shared Hashtree/Iris Git release record; never build or run assets."""
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import sys
import tarfile
import zipfile

MAX_BYTES = 8 * 1024**3
MAX_FILES = 100_000


def relative(value):
    if not isinstance(value, str) or not value or '\\' in value or ':' in value:
        raise ValueError('invalid relative path')
    path = PurePosixPath(value)
    if path.is_absolute() or '..' in path.parts or any(ord(c) < 32 for c in value):
        raise ValueError('unsafe relative path')
    return path


def inside(root, value):
    path = root.joinpath(*relative(value).parts)
    if not path.resolve().is_relative_to(root.resolve()):
        raise ValueError('path escapes release directory')
    return path


def digest(path):
    with path.open('rb') as file:
        value = hashlib.sha256()
        for chunk in iter(lambda: file.read(1024 * 1024), b''):
            value.update(chunk)
        return value.hexdigest()


def version(tag, scheme):
    if scheme == 'date-revision':
        match = re.fullmatch(r'v(\d{4})\.(\d{1,2})\.(\d{1,2})(?:\.(\d{1,2}))?', tag)
        if not match:
            raise ValueError('expected a date release tag')
        year, month, day, revision = (int(part or 0) for part in match.groups())
        import datetime
        datetime.date(year, month, day)
        return f'{year}.{month}.{day * 100 + revision}'
    if scheme != 'semver' or not re.fullmatch(r'v\d+\.\d+\.\d+', tag):
        raise ValueError('expected a stable vX.Y.Z release tag')
    return tag[1:]


def unpack(archive, destination):
    """Reject links/devices/traversals instead of altering a signed app's layout."""
    total = count = 0
    seen = set()

    def write(name, size, mode, stream, directory=False):
        nonlocal total, count
        path = inside(destination, name)
        # AppleDouble entries encode macOS archive metadata, not bundle files.
        # Match the existing package packer's metadata exclusions.
        if any(part == '__MACOSX' or part.startswith('._') for part in relative(name).parts):
            return
        if directory:
            path.mkdir(parents=True, exist_ok=True)
            return
        if path in seen:
            raise ValueError('duplicate archive member')
        seen.add(path)
        total += size
        count += 1
        if size < 0 or total > MAX_BYTES or count > MAX_FILES:
            raise ValueError('archive exceeds package limits')
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open('xb') as file:
            shutil.copyfileobj(stream, file, 1024 * 1024)
        if path.stat().st_size != size:
            raise ValueError('truncated archive member')
        path.chmod(0o755 if mode & 0o111 else 0o644)

    if archive.suffix == '.deb':
        # Debian's ar envelope contains a normal data.tar.{gz,xz,...} archive.
        with archive.open('rb') as source:
            if source.read(8) != b'!<arch>\n':
                raise ValueError('invalid Debian archive')
            while header := source.read(60):
                if len(header) != 60 or header[-2:] != b'`\n':
                    raise ValueError('invalid ar header')
                name = header[:16].decode('ascii').strip().rstrip('/')
                size = int(header[48:58])
                if size < 0 or size > MAX_BYTES:
                    raise ValueError('invalid ar member size')
                if name.startswith('data.tar'):
                    import tempfile
                    with tempfile.TemporaryDirectory() as temp:
                        data = Path(temp) / name
                        with data.open('xb') as output:
                            left = size
                            while left:
                                chunk = source.read(min(left, 1024 * 1024))
                                if not chunk:
                                    raise ValueError('truncated Debian archive')
                                output.write(chunk)
                                left -= len(chunk)
                        unpack(data, destination)
                    return
                source.seek(size + size % 2, 1)
        raise ValueError('Debian archive is missing data.tar')
    if archive.suffix == '.zip':
        with zipfile.ZipFile(archive) as bundle:
            for member in bundle.infolist():
                mode = member.external_attr >> 16
                if stat.S_IFMT(mode) not in (0, stat.S_IFREG, stat.S_IFDIR):
                    raise ValueError('archive contains a link or special file')
                with bundle.open(member) as source:
                    write(member.filename, member.file_size, mode, source, member.is_dir())
    else:
        with tarfile.open(archive, 'r:*') as bundle:
            for member in bundle:
                if not member.isdir() and not member.isfile():
                    raise ValueError('archive contains a link or special file')
                if member.isdir():
                    write(member.name, 0, member.mode, None, True)
                else:
                    with bundle.extractfile(member) as source:
                        write(member.name, member.size, member.mode, source)


def prepare(stage, config, output, tag, check=False):
    record = json.loads((stage / 'release.json').read_text())
    if record.get('tag') != tag or (record.get('draft', False) and not check) or record.get('prerelease', False):
        raise ValueError('release must be the requested final stable tag')
    commit = record.get('commit', '')
    if not re.fullmatch(r'[0-9a-f]{40}|[0-9a-f]{64}', commit):
        raise ValueError('release must pin a full source commit')
    package_version = version(tag, config.get('version_scheme', 'semver'))
    assets = {}
    for asset in record['assets']:
        if asset['name'] in assets:
            raise ValueError('duplicate release asset')
        assets[asset['name']] = asset
    plan = []
    for index, item in enumerate(config['packages']):
        name = item['asset'].replace('{tag}', tag)
        asset = assets.get(name)
        if asset is None:
            if item.get('optional', False):
                continue
            raise ValueError(f'missing release asset: {name}')
        path = inside(stage, asset.get('path', name))
        if not re.fullmatch(r'[0-9a-f]{64}', asset.get('sha256', '')):
            raise ValueError(f'missing asset checksum: {name}')
        if path.stat().st_size != asset['size'] or digest(path) != asset['sha256']:
            raise ValueError(f'release asset checksum/size mismatch: {name}')
        directory = output / str(index)
        directory.mkdir()
        unpack(path, directory)
        payload = inside(directory, item.get('root', '.'))
        if not payload.is_dir():
            raise ValueError('package root is missing')
        spec = {key: value for key, value in item.items()
                if key not in ('asset', 'root', 'optional')}
        spec['version'] = package_version
        spec['source'] = {'git': config['source'], 'rev': commit}
        for command in spec.get('commands', {}).values():
            if not inside(payload, command).is_file():
                raise ValueError(f'package command is missing: {command}')
        if spec.get('app') and not inside(payload, spec['app']).is_dir():
            raise ValueError('package app is missing')
        plan.append({'spec': spec, 'payload': str(payload)})
    if not plan:
        raise ValueError('release contains no configured packages')
    return plan


if __name__ == '__main__':
    stage, config_path, output, tag, mode = sys.argv[1:]
    output = Path(output)
    config = json.loads(Path(config_path).read_text())
    try:
        plan = prepare(Path(stage), config, output, tag, check=mode == 'check')
        (output / 'plan.json').write_text(json.dumps(plan))
    except (ValueError, KeyError, OSError, tarfile.TarError, zipfile.BadZipFile) as error:
        raise SystemExit(f'release preparation failed: {error}') from error
