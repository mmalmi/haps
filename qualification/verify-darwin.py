#!/usr/bin/env python3
"""Verify and extract the official fixed Haps bundle into a new, inactive directory."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile

REPO = 'mmalmi/haps'
TAG = 'v0.1.25'
FIX = '0f00ad99d97b4be3a5280d50eddd2d2daa96630d'
TARGET = 'aarch64-apple-darwin'
ARCHIVE = f'haps-{TAG}-{TARGET}.tar.gz'
MEMBERS = {'haps', 'bundle.json', 'libexec/htree', 'libexec/git-remote-htree', 'libexec/hashtree-LICENSE'}


def command(*args, **kwargs):
    return subprocess.check_output(args, text=True, timeout=180, **kwargs).strip()


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--commit', required=True)
    parser.add_argument('--release-run', type=int, required=True)
    parser.add_argument('--companions', type=Path, required=True, help='libexec staged by scripts/bundle.py from the tag')
    parser.add_argument('--output', type=Path, required=True, help='new directory; existing installs are never overwritten')
    args = parser.parse_args()
    os.umask(0o077)
    require((platform.system(), platform.machine()) == ('Darwin', 'arm64'), 'requires native Apple Silicon')
    require(re.fullmatch('[0-9a-f]{40}', args.commit) and args.release_run > 0, 'expected full commit and run ID')
    require(command('git', 'rev-parse', 'HEAD') == command('git', 'rev-parse', TAG + '^{commit}') == args.commit,
            'checkout must equal the release tag and independently accepted commit')
    subprocess.run(['git', 'merge-base', '--is-ancestor', FIX, args.commit], check=True)
    require(not args.output.exists() and not args.output.is_symlink(), 'output must be a new path')
    target = args.output.absolute()
    require(target.parent.is_dir(), 'output parent must already exist')
    hosted = (os.environ.get('GITHUB_ACTIONS') == 'true' and os.environ.get('RUNNER_ENVIRONMENT') == 'github-hosted'
              and os.environ.get('GITHUB_REPOSITORY') == REPO and os.environ.get('RUNNER_TEMP')
              and target.parent.resolve() == Path(os.environ['RUNNER_TEMP']).resolve())
    reserve = (4 if hosted else 20) * 1024**3

    run = json.loads(command('gh', 'api', f'repos/{REPO}/actions/runs/{args.release_run}'))
    require(run['repository']['full_name'] == REPO and run['path'] == '.github/workflows/release.yml'
            and run['event'] == 'workflow_dispatch' and run['status'] == 'completed' and run['conclusion'] == 'success',
            'invalid official binary release workflow')
    release = json.loads(command('gh', 'api', f'repos/{REPO}/releases/tags/{TAG}'))
    require(release['tag_name'] == TAG and not release['draft'] and not release['prerelease'], 'not an official stable release')
    names = [ARCHIVE, 'SHA256SUMS', 'release.json']
    provider = {a['name']: a for a in release['assets'] if a['name'] in names}
    require(set(provider) == set(names), 'missing provider assets')
    require(shutil.disk_usage(target.parent).free > reserve + 3 * provider[ARCHIVE]['size'] + 384 * 1024**2,
            'not enough free capacity above the runner reserve to verify this archive')
    artifacts = json.loads(command('gh', 'api', f'repos/{REPO}/actions/runs/{args.release_run}/artifacts?per_page=100'))['artifacts']
    native = [a for a in artifacts if a['name'] == 'haps-' + TARGET and not a['expired']]
    require(len(native) == 1 and native[0]['workflow_run']['id'] == args.release_run, 'missing original native run artifact')
    with tempfile.TemporaryDirectory(dir=target.parent, prefix='.haps-verify-') as tmp:
        tmp = Path(tmp)
        for name in names:
            subprocess.run(['gh', 'release', 'download', TAG, '--repo', REPO, '--pattern', name, '--dir', str(tmp)],
                           check=True, timeout=240)
            item = provider[name]
            require(item['state'] == 'uploaded' and (tmp/name).stat().st_size == item['size']
                    and 'sha256:' + digest(tmp/name) == item['digest'], 'incorrect provider asset: ' + name)
        built = tmp/'hosted'
        subprocess.run(['gh','run','download',str(args.release_run),'--repo',REPO,'--name','haps-' + TARGET,'--dir',str(built)],
                       check=True, timeout=240)
        require({f.name for f in built.iterdir()} == {ARCHIVE} and (built/ARCHIVE).is_file()
                and not (built/ARCHIVE).is_symlink() and digest(built/ARCHIVE) == provider[ARCHIVE]['digest'][7:],
                'published archive differs from this hosted native run')
        info = json.loads((tmp/'release.json').read_text())
        require((info['schema'], info['tag'], info['version'], info['commit']) ==
                ('haps.binary-release.v1', TAG, TAG[1:], args.commit), 'manifest identity differs')
        pins = json.loads(command('git', 'show', args.commit + ':scripts/hashtree-bundle.json'))
        require(info['companions'] == pins, 'companion pins differ from source')
        files = [entry for entry in info['assets'] if entry['target'] == TARGET]
        require(len(files) == 1 and files[0]['name'] == ARCHIVE and files[0]['size'] == provider[ARCHIVE]['size']
                and 'sha256:' + files[0]['sha256'] == provider[ARCHIVE]['digest'], 'manifest archive mismatch')
        checksum = f"{files[0]['sha256']}  {ARCHIVE}"
        require((tmp/'SHA256SUMS').read_text().splitlines().count(checksum) == 1, 'checksum file mismatch')

        out = tmp/'tools'; out.mkdir(); (out/'libexec').mkdir()
        with tarfile.open(tmp/ARCHIVE, 'r:gz') as tar:
            members = tar.getmembers()
            require(len(members) == len(MEMBERS) and {m.name for m in members} == MEMBERS, 'unexpected archive members')
            require(shutil.disk_usage(target.parent).free > reserve + sum(m.size for m in members) + 64 * 1024**2,
                    'not enough capacity to extract the verified archive')
            for m in members:
                require(m.isfile() and 0 < m.size <= 256 * 1024**2, 'invalid archive member: ' + m.name)
                f = out/m.name
                with tar.extractfile(m) as src, f.open('xb') as dest:
                    shutil.copyfileobj(src, dest, length=1024 * 1024)
                require(f.stat().st_size == m.size, 'incomplete member')
                if m.name in ('haps', 'libexec/htree', 'libexec/git-remote-htree'):
                    require(m.mode & 0o111 != 0, 'non-executable tool: ' + m.name)
                    f.chmod(0o755)
        component_digests = {name: digest(out/name) for name in sorted(MEMBERS)}
        companion_pin = pins['assets'][TARGET]
        bundle = json.loads((out/'bundle.json').read_text())
        require(bundle == {'hashtree': pins['tag'], 'archive_sha256': companion_pin['sha256']}, 'wrong target companion bundle')
        require(json.loads((args.companions.parent/'bundle.json').read_text()) == bundle,
                'companion comparison directory is not from the pinned target archive')
        for name in ('htree', 'git-remote-htree'):
            require(component_digests['libexec/'+name] == digest(args.companions/name), 'bundled companion mismatch')
        require(component_digests['libexec/git-remote-htree'] == companion_pin['git_remote_htree_sha256'],
                'git helper does not match full tag pin')

        env = dict(os.environ, HAPS_HOME=str(tmp/'private-home'), HTREE_CONFIG_DIR=str(tmp/'isolated-config'),
                   HTREE_DATA_DIR=str(tmp/'isolated-data'), NOSTR_RELAYS='ws://127.0.0.1:9')
        require(command(str(out/'haps'), '--version', env=env) == 'haps ' + TAG[1:], 'incorrect Haps version')
        require(command(str(out/'haps'), 'target', env=env) == TARGET, 'incorrect Haps target')
        help_text = command(str(out/'haps'), 'import-release', '--help', env=env)
        require(all(opt in help_text for opt in ('--check', '--publish-source', '--publish')), 'missing release operations')
        require(command(str(out/'libexec/htree'), '--version', env=env) == 'htree ' + pins['tag'][1:], 'wrong bundled htree')
        receipt = {'tag': TAG, 'commit': args.commit, 'fix': FIX, 'target': TARGET, 'source_tree': command('git','rev-parse',args.commit+'^{tree}'),
                   'official_run': run['html_url'], 'native_artifact_id': native[0]['id'], 'native_artifact_digest': native[0]['digest'],
                   'release': release['html_url'], 'archive': ARCHIVE, 'size': files[0]['size'],
                   'provider_assets': {name: {'id': provider[name]['id'], 'sha256': provider[name]['digest'][7:]} for name in names},
                   'runner_reserve_bytes': reserve, 'bundle_members_sha256': component_digests, 'verifier_sha256': digest(Path(__file__).resolve())}
        (out/'qualification.json').write_text(json.dumps(receipt, indent=2) + '\n')
        require(not target.exists() and not target.is_symlink(), 'destination became occupied')
        out.rename(target)
        print(json.dumps(receipt, indent=2))


if __name__ == '__main__':
    main()
