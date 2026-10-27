"""Omarchy package menus, also usable as a standalone terminal picker."""
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile

START = '  // BEGIN HAPS MENU\n'
END = '  // END HAPS MENU\n'
MENU_ROOTS = ('haps', 'install.haps', 'update.haps', 'remove.haps')

SEARCH_FORMATTER = r'''
import json
import sys

def text(value):
    return ''.join(c if c.isprintable() else ' ' for c in str(value))

rows = json.load(sys.stdin)
installed = len(sys.argv) > 1 and sys.argv[1] == 'list'
if not rows:
    print('No installed packages.' if installed else 'No matching packages. Try another search.')
for row in rows:
    package = row['package']
    print(text(row['label']) + '  ' + text(package['version']))
    print('  ' + text(package.get('description', '')))
    print('  ' + text(package['target']) + ' · Audits: ' + str(len(row.get('audits', [])))
          + ' · Warnings: ' + str(len(row.get('warnings', []))))
    print()
'''

# Embedded in the generated menu helper so Cargo installs need no extra files.
PACKAGE_PICKER = r'''
import hashlib
import json
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time

SEARCH_POLL_SECONDS = 5
SEARCH_TIMEOUT_SECONDS = 120

def text(value):
    return ''.join(c if c.isprintable() else ' ' for c in str(value))

def search(haps):
    print('Finding packages… Ctrl-C to cancel.', flush=True)
    started = time.monotonic()
    with subprocess.Popen(haps + ['search', '--json', '--', ''],
                          stdout=subprocess.PIPE, text=True) as process:
        try:
            while True:
                remaining = SEARCH_TIMEOUT_SECONDS - (time.monotonic() - started)
                if remaining <= 0:
                    raise TimeoutError('Package search timed out. Check your connection and try again.')
                try:
                    output, _ = process.communicate(timeout=min(SEARCH_POLL_SECONDS, remaining))
                    if process.returncode:
                        print(text(output), file=sys.stderr)
                        raise subprocess.CalledProcessError(process.returncode, process.args)
                    return json.loads(output)
                except subprocess.TimeoutExpired:
                    elapsed = int(time.monotonic() - started)
                    print('Still finding packages (' + str(elapsed) + 's)… Ctrl-C to cancel.',
                          file=sys.stderr, flush=True)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()


def search_rows(rows, target):
    latest = {}
    for row in rows:
        package = row['package']
        version = package['version'].split('+', 1)[0]
        if package['target'] != target or '-' in version:
            continue
        order = tuple(int(part) for part in version.split('.'))
        identity = row['identity']
        if identity not in latest or order > latest[identity][0]:
            latest[identity] = (order, row)
    return [entry[1] for entry in latest.values()]


def cache_context(haps):
    # The installed integration supplies the exact CLI home. Standalone callers
    # without one can still search, but must not share another profile's cache.
    if '--home' not in haps:
        return None
    home = Path(haps[haps.index('--home') + 1])
    digest = hashlib.sha256(json.dumps(haps).encode())
    for name in ('config.json', 'social.json'):
        path = home / name
        digest.update(path.read_bytes() if path.exists() else b'')
        digest.update(b'\0')
    return home / 'integrations/omarchy-search.json', digest.hexdigest()


def saved_search(haps, target):
    try:
        context = cache_context(haps)
        if context is None:
            return None
        path, fingerprint = context
        saved = json.loads(path.read_text())
        if (saved['schema'] != 1 or saved['fingerprint'] != fingerprint
                or saved['target'] != target):
            return None
        rows = search_rows(saved['rows'], target)
        # Empty snapshots should not hide packages arriving after initial setup.
        if not rows:
            return None
        label = 'Saved list from ' + time.strftime('%Y-%m-%d %H:%M', time.localtime(saved['time']))
        return rows, label
    except (OSError, ValueError, KeyError, TypeError, OverflowError):
        return None


def save_search(haps, target, rows):
    temporary = None
    try:
        context = cache_context(haps)
        if context is None:
            return
        path, fingerprint = context
        path.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(mode='w', dir=path.parent, delete=False) as file:
            temporary = Path(file.name)
            json.dump(dict(schema=1, fingerprint=fingerprint, target=target,
                           time=time.time(), rows=rows), file)
        temporary.replace(path)
    except OSError as error:
        print('Could not save package list: ' + text(error), file=sys.stderr)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def run():
    action, *haps = sys.argv[1:]
    searching = action == 'search'
    label = ''
    if searching:
        target = subprocess.check_output(haps + ['target'], text=True).strip()
        saved = saved_search(haps, target)
        if saved is not None:
            rows, label = saved
        else:
            rows = search_rows(search(haps), target)
            save_search(haps, target, rows)
    else:
        result = subprocess.run(haps + ['list', '--json'], stdout=subprocess.PIPE, text=True)
        if result.returncode:
            print(text(result.stdout), file=sys.stderr)
            return result.returncode
        rows = json.loads(result.stdout)
    return pick(action, haps, rows, label, target if searching else None)


def pick(action, haps, rows, status, target):
    searching = action == 'search'
    if not rows:
        print('No packages found for this computer.' if searching else 'No installed packages.')
        return 0
    operation = 'install' if searching else action
    while True:
        with tempfile.TemporaryDirectory(prefix='haps-picker-') as directory:
            choices = []
            for index, row in enumerate(rows):
                package = row['package']
                label = text(row['label'])
                version = text(package['version'])
                description = text(package.get('description', ''))
                choices.append(str(index) + '\t' + label + '\t' + version + '\t' + description)
                preview = [label + '  ' + version, description, '',
                           'Platform: ' + text(package['target']),
                           'Publisher: ' + text(row['publisher']),
                           'Release: ' + text(row['release_id']),
                           'Audits: ' + str(len(row.get('audits', []))),
                           'Warnings: ' + str(len(row.get('warnings', []))), '',
                           'Haps checks trust before installing or updating.']
                if status:
                    preview += ['', status + '. Refresh to update these details.']
                Path(directory, str(index)).write_text('\n'.join(preview) + '\n')
            picked = subprocess.run([
                'fzf', '--delimiter=\t', '--with-nth=2..', '--layout=reverse',
                '--prompt=Haps > ',
                '--header=' + (status + ' · ' if status else '') + 'Enter: ' + operation
                    + (' · Ctrl-R: refresh' if searching else '') + ' · Esc: cancel',
                '--preview=cat ' + shlex.quote(directory) + '/{1}',
                '--preview-window=down:65%:wrap',
                '--preview-label=alt-p: toggle details, alt-j/k: scroll',
                '--preview-label-pos=bottom',
                '--bind=alt-p:toggle-preview,alt-j:preview-down,alt-k:preview-up',
            ] + (['--expect=ctrl-r'] if searching else []), input='\n'.join(choices) + '\n', stdout=subprocess.PIPE, text=True)
            if picked.returncode:
                return 130 if picked.returncode in (1, 130) else picked.returncode
            # Only a row from this snapshot can become an argument; labels are display-only.
            selected = picked.stdout.rstrip('\n')
            if searching and selected.split('\n', 1)[0] == 'ctrl-r':
                try:
                    refreshed = search_rows(search(haps), target)
                    save_search(haps, target, refreshed)
                    if not refreshed:
                        print('No packages found for this computer.')
                        return 0
                    rows = refreshed
                    status = ''
                except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
                    print('Refresh failed: ' + text(error), file=sys.stderr)
                    status = 'Refresh failed; showing previous list'
                continue
            if searching and selected.startswith('\n'):
                selected = selected[1:]
            if selected not in choices:
                raise ValueError('The package selection was not in the result list.')
            row = rows[int(selected.split('\t', 1)[0])]
        break
    args = [operation]
    if searching:
        args += ['--version', row['package']['version']]
    return subprocess.call(haps + args + ['--', row['identity']])

try:
    sys.exit(run())
except subprocess.CalledProcessError as error:
    sys.exit(error.returncode)
except KeyboardInterrupt:
    sys.exit(130)
except (OSError, ValueError, KeyError, TypeError) as error:
    print(str(error), file=sys.stderr)
    sys.exit(1)
'''

def menu_script(executable, home=None):
    command = [executable] + (['--home', home] if home is not None else [])
    search = ('show_packages() {\n  local results\n'
              '  [[ "$1" != search ]] || printf \'Finding packages…\\n\'\n'
              '  results=$("${haps[@]}" "$@") || return $?\n'
              '  printf \'%s\\n\' "$results" | python3 -c ' + shlex.quote(SEARCH_FORMATTER) + ' "$1"\n}\n')
    picker = 'pick_package() { python3 -c ' + shlex.quote(PACKAGE_PICKER) + ' "$1" "${haps[@]}"; }\n'
    return '#!/usr/bin/env bash\nset -u\nhaps=(' + shlex.join(command) + ')\n' + search + picker + r'''action=${1:-search}
if [[ "$action" == menu ]]; then
  selected=$(printf '%s\n' $'search\tFind and install packages' $'install\tInstall by name' $'list\tInstalled packages' $'update\tUpdate a package' $'remove\tRemove a package' |
    fzf --delimiter=$'\t' --with-nth=2 --prompt='Haps > ' --header='Packages from your social graph' --height=12 --layout=reverse) || exit 0
  action=${selected%%$'\t'*}
fi
case "$action" in
  search|update|remove) pick_package "$action" ;;
  install) read -r -p 'Package (name or publisher/name): ' package || exit 0; [[ -n "$package" ]] || exit 0; "${haps[@]}" install -- "$package" ;;
  list) show_packages list --json ;;
  *) exit 2 ;;
esac
result=$?
[[ "$result" == 130 ]] && exit 0
printf '\n'
read -r -p 'Press Enter to close.' _
exit "$result"
'''

def install_directory():
    return Path(os.environ.get('HAPS_INSTALL_DIR') or Path.home() / '.local/bin').expanduser().absolute()

def find_haps():
    executable = shutil.which('haps')
    if executable:
        return executable
    # Desktop sessions may not inherit the user's interactive shell PATH.
    for directory in [install_directory(), Path.home() / '.cargo/bin']:
        candidate = directory / 'haps'
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    return None

def pause():
    try:
        input('\nPress Enter to close.')
    except (EOFError, KeyboardInterrupt):
        pass

def terminal_menu(executable=None, home=None):
    executable = executable or find_haps()
    if not executable:
        raise ValueError('Install Haps before opening its package menu: https://haps.hashtree.cc')
    if not shutil.which('fzf'):
        raise ValueError('The Haps menu needs fzf. Install it with: omarchy pkg add fzf')
    return subprocess.call(['bash', '-c', menu_script(executable, home), 'haps-menu', 'menu'])

def parse(raw):
    # Match Omarchy v4's supported JSONC: full-line comments and trailing commas.
    clean = re.sub(r'^\s*//[^\n]*(\n|$)', lambda m: ' ' * (len(m[0]) - m[0].count('\n')) + '\n' * m[0].count('\n'), raw, flags=re.M)
    clean = re.sub(r',(?=\s*[}\]])', ' ', clean)
    value = json.loads(clean)
    if not isinstance(value, dict):
        raise ValueError('Omarchy menu must be a JSON object')
    return clean, value

def merge(raw, rows):
    if START in raw or END in raw:
        if raw.count(START) != 1 or raw.count(END) != 1 or raw.index(START) > raw.index(END):
            raise ValueError('Haps menu markers were edited; restore them before retrying')
        raw = raw[:raw.index(START)] + raw[raw.index(END) + len(END):]
    clean, parsed = parse(raw)
    entries = parsed.get('items', parsed)
    if not isinstance(entries, dict):
        raise ValueError('Omarchy menu items must be an object')
    if any(k == root or k.startswith(root + '.') for k in entries for root in MENU_ROOTS):
        raise ValueError('Existing Haps menu entries are not managed by this installer')
    if not rows:
        return raw
    opening = clean.index('{')
    if 'items' in parsed:
        decoder = json.JSONDecoder()
        cursor = opening + 1
        while True:
            cursor = re.compile(r'\s*').match(clean, cursor).end()
            key, cursor = decoder.raw_decode(clean, cursor)
            cursor = re.compile(r'\s*:\s*').match(clean, cursor).end()
            if key == 'items':
                opening = cursor
                break
            _, cursor = decoder.raw_decode(clean, cursor)
            cursor = re.compile(r'\s*,\s*').match(clean, cursor).end()
    block = START + ''.join('  ' + json.dumps(k) + ': ' + json.dumps(v, ensure_ascii=False) + ',\n' for k, v in rows.items()) + END
    # No bytes outside our marked block change on subsequent install/remove.
    # A newline after the opening brace also makes the comment valid JSONC.
    if raw[opening + 1:opening + 2] != '\n':
        raw = raw[:opening + 1] + '\n' + raw[opening + 1:]
    result = raw[:opening + 2] + block + raw[opening + 2:]
    parse(result)
    return result

def atomic(path, text, mode):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as file:
            file.write(text)
            file.flush()
            os.fsync(file.fileno())
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)

def install(executable, home, remove=False):
    # Omarchy itself reads HOME/.config, regardless of XDG_CONFIG_HOME.
    menu = Path.home() / '.config/omarchy/extensions/omarchy-menu.jsonc'
    if not menu.parent.exists() and not remove:
        raise ValueError('Omarchy extensions directory is missing; this integration needs Omarchy v4')
    helper = Path(home) / 'integrations/omarchy-menu'
    raw = menu.read_text() if menu.exists() else '{\n}\n'
    rows = {}
    for parent, action in [('install', 'search'), ('update', 'update'), ('remove', 'remove')]:
        command = shlex.join([str(helper), action])
        rows[parent + '.haps'] = {'label': 'Haps', 'icon': '󰏗',
                                  'description': 'Packages from your social graph',
                                  'action': shlex.join(['omarchy-launch-or-focus-tui', '--app-id=to.hashtree.haps.' + action, command])}
    rows['install.haps']['aliases'] = ['haps']
    changed = merge(raw, {} if remove else rows)
    backup = menu.with_suffix('.jsonc.haps-backup')
    if menu.exists() and not backup.exists():
        # Preserve the exact original once. Never overwrite a prior backup.
        with backup.open('x') as file:
            file.write(raw)
    if not remove:
        atomic(helper, menu_script(executable, home), 0o755)
    atomic(menu, changed, menu.stat().st_mode & 0o777 if menu.exists() else 0o644)
    if remove:
        helper.unlink(missing_ok=True)
    print('Haps menu removed.' if remove else 'Haps added to Install, Update, and Remove in the Omarchy menu.')

if __name__ == '__main__':
    result = 0
    try:
        if sys.argv[1:] == ['--menu']:
            result = terminal_menu()
        elif len(sys.argv) == 4 and sys.argv[3] == '--menu':
            result = terminal_menu(sys.argv[1], sys.argv[2])
        elif len(sys.argv) >= 3:
            install(sys.argv[1], sys.argv[2], '--remove' in sys.argv[3:])
        else:
            raise ValueError('Usage: omarchy.py --menu | EXECUTABLE HOME [--remove]')
    except (OSError, ValueError) as error:
        print(str(error), file=sys.stderr)
        result = 1
        if sys.argv[1:] == ['--menu']:
            pause()
    except KeyboardInterrupt:
        result = 130
    sys.exit(result)
