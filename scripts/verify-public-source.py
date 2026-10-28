#!/usr/bin/env python3
"""Cold-fetch public release source before CI publishes any binary assets."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

PUBLIC_READ_SERVERS = ('https://cdn.iris.to', 'https://upload.iris.to')


def clone_timeout_seconds(value):
    try:
        seconds = int(value)
    except (TypeError, ValueError, OverflowError):
        raise argparse.ArgumentTypeError('Clone timeout must be an integer from 1 to 1800 seconds') from None
    if not 1 <= seconds <= 1800:
        raise argparse.ArgumentTypeError('Clone timeout must be from 1 to 1800 seconds')
    return seconds


def verify(source, revision, helpers, clone_timeout=300, read_servers=None):
    clone_timeout = clone_timeout_seconds(clone_timeout)
    servers = sorted(set(read_servers or ()))
    if any(server not in PUBLIC_READ_SERVERS for server in servers):
        raise ValueError('Read servers must be https://cdn.iris.to or https://upload.iris.to')
    if not source.startswith('htree://') or not re.fullmatch(r'[0-9a-fA-F]{40}|[0-9a-fA-F]{64}', revision):
        raise ValueError('Expected an explicit Hashtree source URL and full commit hash')
    with tempfile.TemporaryDirectory(prefix='public-source-') as temporary:
        root = Path(temporary)
        if servers:
            config = root / 'config'
            config.mkdir()
            (config / 'config.toml').write_text(
                '[blossom]\nservers = []\nread_servers = ' + json.dumps(servers)
                + '\nwrite_servers = []\n', encoding='utf-8')
            print('Public source read servers:', ', '.join(servers),
                  f'(clone timeout: {clone_timeout}s)', flush=True)
        env = dict(os.environ, HTREE_CONFIG_DIR=str(root / 'config'),
                   HTREE_DATA_DIR=str(root / 'data'), HTREE_PREFER_LOCAL_DAEMON='false',
                   HTREE_LOCAL_DAEMON_ONLY='false', GIT_CONFIG_NOSYSTEM='1',
                   GIT_CONFIG_GLOBAL=os.devnull, GIT_NO_REPLACE_OBJECTS='1',
                   GIT_TERMINAL_PROMPT='0', PATH=str(helpers.resolve()) + os.pathsep + os.environ['PATH'])
        for name in ('GIT_CONFIG_COUNT', 'GIT_CONFIG_PARAMETERS', 'GIT_DIR', 'GIT_WORK_TREE',
                     'GIT_INDEX_FILE', 'GIT_OBJECT_DIRECTORY', 'GIT_ALTERNATE_OBJECT_DIRECTORIES',
                     'GIT_SHALLOW_FILE', 'NOSTR_RELAYS'):
            env.pop(name, None)
        command = ['git', '-c', 'protocol.ext.allow=never']
        subprocess.run([*command, 'clone', '--no-local', '--no-checkout', '--', source, str(root / 'repo')],
                       check=True, env=env, timeout=clone_timeout)
        def git(*args):
            return subprocess.check_output([*command, '-C', str(root / 'repo'), *args],
                                           env=env, text=True, timeout=120).strip()
        if git('rev-parse', '--verify', revision + '^{commit}').lower() != revision.lower():
            raise ValueError('Public source does not resolve to release commit')
        if git('rev-parse', '--is-shallow-repository') != 'false':
            raise ValueError('Public source has incomplete history')
        git('rev-list', '--objects', '--missing=error', revision, '--')
        print('Public Hashtree source and complete history verified:', revision)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, default=Path('haps-release.json'))
    parser.add_argument('--commit', required=True)
    parser.add_argument('--helpers', type=Path, required=True)
    parser.add_argument('--clone-timeout', type=clone_timeout_seconds, default=300, metavar='SECONDS',
                        help='Cold clone deadline, from 1 to 1800 seconds (default: 300)')
    parser.add_argument('--read-server', action='append', choices=PUBLIC_READ_SERVERS,
                        help='Public read server; repeat to select both (default: helper defaults)')
    args = parser.parse_args()
    verify(json.loads(args.config.read_text())['source'], args.commit, args.helpers,
           args.clone_timeout, args.read_server)
