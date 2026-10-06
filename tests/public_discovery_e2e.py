"""Opt-in public publish/discover check; run each role in a separate container.

This publishes one harmless, uniquely named test package. It is deliberately
outside normal test discovery so CI does not fill public relays with fixtures.
Only the publisher's public proof is passed to the reader on stdin; no catalog
URL, signing key, local cache, or package files cross between the two users.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('role', choices=['publish', 'read', 'review', 'read-feedback'])
    args = parser.parse_args()
    if not Path('/.dockerenv').exists() and os.environ.get('HAPS_TEST_ISOLATED') != '1':
        parser.error('run inside a disposable container or VM')
    with tempfile.TemporaryDirectory(prefix='haps-user-') as tmp:
        root = Path(tmp)
        home = root / 'haps'
        env = dict(os.environ, HAPS_HOME=str(home), HAPS_NO_DEFAULTS='true',
                   HAPS_NON_INTERACTIVE='true', HTREE_CONFIG_DIR=str(root / 'hashtree'),
                   HTREE_PREFER_LOCAL_DAEMON='false', HTREE_LOCAL_DAEMON_ONLY='false',
                   NOSTR_RELAYS='wss://relay.snort.social,wss://temp.iris.to',
                   XDG_DATA_HOME=str(root / 'data'), XDG_CONFIG_HOME=str(root / 'config'))
        (root / 'hashtree').mkdir()
        (root / 'hashtree/config.toml').write_text(
            '[storage]\ndata_dir = ' + json.dumps(str(root / 'hashtree/data')) +
            '\n[nostr]\nrelays = ["wss://relay.snort.social", "wss://temp.iris.to"]\n')

        def run(*args, check=True):
            result = subprocess.run(['haps', *args], cwd=root, env=env,
                                    capture_output=True, text=True, timeout=180)
            if check and result.returncode:
                raise RuntimeError(f'haps {args[0]} failed: {result.stderr[-2000:]}')
            return result

        identity = run('identity', 'init').stdout.strip()
        config = json.loads((home / 'config.json').read_text())
        assert not config['sources'] and not config['indexes']
        if args.role == 'publish':
            name = 'haps-e2e-' + uuid.uuid4().hex[:12]
            message = 'Haps two-user discovery proof ' + uuid.uuid4().hex
            payload = root / 'stage'
            payload.mkdir()
            content = '#!/bin/sh\nprintf "%s\\n" ' + json.dumps(message) + '\n'
            (payload / name).write_text(content)
            (payload / name).chmod(0o755)
            run('init', '--name', name, '--version', '1.0.0', '--command', name,
                '--payload', str(payload), '--description',
                'Harmless temporary Haps publish/discovery acceptance fixture')
            release = json.loads(run('pack', 'haps.toml', '--payload', str(payload),
                                     '--out', 'catalog').stdout)
            assert release['pubkey'] == identity
            run('publish', 'catalog', '--name', name)
            pending = list((home / 'discovery/outbox').glob('*.json'))
            if pending:
                run('sync')
            assert not list((home / 'discovery/outbox').glob('*.json')), 'relay did not acknowledge publication'
            print(json.dumps({'name': name, 'publisher': identity, 'release_id': release['id'],
                              'message': message, 'payload_sha256': hashlib.sha256(content.encode()).hexdigest()}))
        elif args.role in ('review', 'read-feedback'):
            proof = json.load(sys.stdin)
            assert identity != proof['publisher']
            package = proof['publisher'] + '/' + proof['name']
            if args.role == 'review':
                comment = run('comment', package,
                              'Automated three-user transport test; this is a harmless fixture.').stdout.strip()
                warning = json.loads(run('warn', proof['release_id'], '--note',
                    'Automated test warning only; no security finding is claimed.', '--json').stdout)
                assert not list((home / 'discovery/outbox').glob('*.json'))
                print(json.dumps(dict(proof, reviewer=identity, comment=comment, warning=warning['id'])))
            else:
                assert identity != proof['reviewer']
                before = json.loads(run('info', package, '--json').stdout)
                assert before['warnings'] == [], 'unfollowed warning affected the reader'
                run('follow', proof['reviewer'])
                thread = run('comments', package).stdout
                assert proof['comment'] in thread and proof['reviewer'] in thread
                info = json.loads(run('info', package, '--json').stdout)
                assert len(info['warnings']) == 1
                assert info['warnings'][0]['event']['id'] == proof['warning']
                denied = run('install', package, '--allow-untrusted', '--json', check=False)
                assert denied.returncode != 0 and 'trusted release warnings' in denied.stdout
                config = json.loads((home / 'config.json').read_text())
                assert all(k.startswith('discovered-') for k in config['sources'])
                print(json.dumps({'status': 'passed', 'publisher': proof['publisher'],
                    'reviewer': proof['reviewer'], 'reader': identity,
                    'comment_visible': True, 'unfollowed_warning_ignored': True,
                    'followed_warning_visible': True, 'warning_blocked_install': True,
                    'manual_sources': 0, 'manual_event_imports': 0}))
        else:
            proof = json.load(sys.stdin)
            assert identity != proof['publisher']
            found = json.loads(run('search', proof['name'], '--json').stdout)
            matching = [r for r in found if r['publisher'] == proof['publisher']
                        and r['package']['name'] == proof['name']]
            assert len(matching) == 1, 'fresh reader did not discover the publisher'
            package = matching[0]
            assert package['release_id'] == proof['release_id']
            assert package['label'].startswith('npub1')
            # Discovery alone must not silently authorize a stranger's code.
            denied = run('install', package['label'], '--json', check=False)
            assert denied.returncode != 0 and 'untrusted' in denied.stdout.lower(), denied.stdout
            run('follow', proof['publisher'])
            installed = json.loads(run('install', package['label'], '--json').stdout)
            assert installed['release']['release_id'] == proof['release_id']
            assert run('run', proof['name']).stdout.strip() == proof['message']
            directory = Path(run('path', proof['name']).stdout.strip())
            assert hashlib.sha256((directory / proof['name']).read_bytes()).hexdigest() == proof['payload_sha256']
            config = json.loads((home / 'config.json').read_text())
            assert config['sources'] and all(k.startswith('discovered-') for k in config['sources'])
            print(json.dumps({'status': 'passed', 'reader': identity, 'publisher': proof['publisher'],
                              'name': proof['name'], 'release_id': proof['release_id'],
                              'manual_sources': 0, 'found_by': 'name search',
                              'unknown_publisher_blocked': True, 'installed_and_executed': True}))


if __name__ == '__main__':
    main()
