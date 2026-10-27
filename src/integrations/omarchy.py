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
import http.client
import json
import os
from pathlib import Path
import secrets
import selectors
import signal
import subprocess
import sys
import tempfile
import time

SEARCH_TIMEOUT_SECONDS = 120
MAX_SNAPSHOT_BYTES = 8 * 1024 * 1024


def text(value):
    return ''.join(c if c.isprintable() else ' ' for c in str(value))


def search_rows(rows, target):
    if not isinstance(rows, list):
        raise ValueError('Package search did not return a result array.')
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


def choices_for(rows, directory):
    choices = {}
    for row in rows:
        package = row['package']
        token = hashlib.sha256(json.dumps([
            row['identity'], row['release_id'], package['version']]).encode()).hexdigest()
        label = text(row['label'])
        version = text(package['version'])
        description = text(package.get('description', ''))
        choice = token + '\t' + label + '\t' + version + '\t' + description
        choices[choice] = row
        preview = [label + '  ' + version, description, '',
                   'Platform: ' + text(package['target']),
                   'Publisher: ' + text(row['publisher']),
                   'Release: ' + text(row['release_id']),
                   'Audits: ' + str(len(row.get('audits', []))),
                   'Warnings: ' + str(len(row.get('warnings', []))), '',
                   'Haps checks trust before installing or updating.']
        Path(directory, token).write_text('\n'.join(preview) + '\n')
    return choices


def picker_args(operation, searching):
    return [
        'fzf', '--delimiter=\t', '--with-nth=2..', '--layout=reverse',
        '--no-multi', '--no-select-1', '--no-exit-0', '--prompt=Haps > ',
        '--header=' + header(operation, searching),
        '--preview=cat "$HAPS_PICKER_DIR"/{1}',
        '--preview-window=down:65%:wrap',
        '--preview-label=alt-p: toggle details, alt-j/k: scroll',
        '--preview-label-pos=bottom',
        '--bind=alt-p:toggle-preview,alt-j:preview-down,alt-k:preview-up',
    ]


def header(operation, searching, status=''):
    return (status + ' · ' if status else '') + 'Enter: ' + operation + (
        ' · Ctrl-R: refresh' if searching else '') + ' · Esc: cancel'


def stop(process, group=False):
    if process is None:
        return
    if group:
        # Reap helpers even when the search process has already exited.
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    elif process.poll() is None:
        process.terminate()
    try:
        process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        process.kill()
    if group:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    process.wait()


def update_picker(port, key, actions):
    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=0.2)
    try:
        connection.request('POST', '/', actions.encode(), {'x-api-key': key})
        response = connection.getresponse()
        response.read()
        if response.status != 200:
            raise OSError('Could not refresh package picker.')
    finally:
        connection.close()


def search_pick(haps, target):
    query = ''
    help_text = subprocess.check_output(['fzf', '--help'], text=True)
    tracking = ['--track', '--id-nth=1'] if '--id-nth' in help_text else []
    while True:
        with tempfile.TemporaryDirectory(prefix='haps-picker-') as directory:
            root = Path(directory)
            key = secrets.token_hex(32)
            env = dict(os.environ, HAPS_PICKER_DIR=directory, FZF_API_KEY=key)
            args = picker_args('install', True) + tracking + [
                '--listen=127.0.0.1:0', '--expect=ctrl-r', '--print-query', '--query=' + query,
                '--bind=load:transform-header:cat "$HAPS_PICKER_DIR/header"',
                '--bind=start:execute-silent:printf "%s" "$FZF_PORT" > "$HAPS_PICKER_DIR/port"',
            ]
            choices, buffer, picked, generation = {}, b'', bytearray(), 0
            pending, port, search = None, None, None
            status = 'Finding packages…'
            (root / 'header').write_text(header('install', True, status))
            failure = None
            with tempfile.TemporaryFile() as errors, selectors.DefaultSelector() as selector:
                picker = subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, env=env)
                try:
                    selector.register(picker.stdout, selectors.EVENT_READ, 'picker')
                    search = subprocess.Popen(haps + ['search', '--json', '--stream', '--', ''],
                                              stdout=subprocess.PIPE, stderr=errors, start_new_session=True)
                    selector.register(search.stdout, selectors.EVENT_READ, 'search')
                    started = time.monotonic()
                    searching, eof = True, False
                    pending = 'change-header(' + header('install', True, status) + ')'
                    while picker.poll() is None:
                        if port is None and (root / 'port').exists():
                            raw = (root / 'port').read_text()
                            if raw.isdecimal() and 0 < int(raw) < 65536:
                                port = int(raw)
                        if port is not None and pending is not None:
                            (root / 'header').write_text(header('install', True, status))
                            try:
                                update_picker(port, key, pending)
                                pending = None
                            except OSError:
                                if picker.poll() is not None:
                                    break
                        for event, _ in selector.select(timeout=0.05):
                            data = os.read(event.fileobj.fileno(), 65536)
                            if event.data == 'picker':
                                picked.extend(data)
                                if not data:
                                    selector.unregister(event.fileobj)
                                continue
                            if data:
                                buffer += data
                                if len(buffer) > MAX_SNAPSHOT_BYTES:
                                    raise ValueError('Package search result is too large.')
                            # An EOF may finish a final line without a newline.
                            lines = buffer.split(b'\n')
                            buffer = lines.pop() if data else b''
                            for line in lines:
                                if not line.strip():
                                    continue
                                rows = search_rows(json.loads(line), target)
                                choices = choices_for(rows, directory)
                                generation += 1
                                snapshot = 'snapshot-' + str(generation)
                                (root / snapshot).write_text('\n'.join(choices) + ('\n' if choices else ''))
                                status = 'Updating packages…' if choices else 'Finding packages…'
                                pending = ('reload-sync(cat "$HAPS_PICKER_DIR/' + snapshot + '")'
                                           '+change-header(' + header('install', True, status) + ')')
                            if not data:
                                selector.unregister(event.fileobj)
                                eof = True
                        if searching and eof and search.poll() is not None:
                            searching = False
                            if search.returncode:
                                failure = 'Package search failed. Press Ctrl-R to retry.'
                            status = failure or ('' if choices else 'No packages found for this computer.')
                            pending = ((pending + '+') if pending else '') + (
                                'change-header(' + header('install', True, status) + ')')
                        if searching and time.monotonic() - started > SEARCH_TIMEOUT_SECONDS:
                            if not eof:
                                selector.unregister(search.stdout)
                            stop(search, group=True)
                            searching = False
                            failure = 'Package search timed out. Press Ctrl-R to retry.'
                            status = failure
                            pending = ((pending + '+') if pending else '') + (
                                'change-header(' + header('install', True, failure) + ')')
                    picked.extend(picker.stdout.read())
                finally:
                    stop(search, group=True)
                    stop(picker)
                    if search is not None:
                        search.stdout.close()
                    picker.stdout.close()
                output = picked.decode().rstrip('\n').split('\n', 2)
                query = output[0]
                # fzf returns 1 for an expected key when the result list is empty.
                if picker.returncode in (0, 1) and len(output) > 1 and output[1] == 'ctrl-r':
                    continue
                if picker.returncode:
                    if failure:
                        print(failure, file=sys.stderr)
                    return 130 if picker.returncode in (1, 130) else picker.returncode
                selected = output[2] if len(output) == 3 else ''
                # A newer snapshot can arrive while Enter is being handled. Never
                # reinterpret an old row number or install an invalidated choice.
                if selected not in choices:
                    raise ValueError('Package results changed. Open the picker and select again.')
                row = choices[selected]
                return subprocess.call(haps + ['install', '--version', row['package']['version'],
                                               '--release', row['release_id'], '--', row['identity']])


def run():
    action, *haps = sys.argv[1:]
    if action == 'search':
        target = subprocess.check_output(haps + ['target'], text=True).strip()
        return search_pick(haps, target)
    result = subprocess.run(haps + ['list', '--json'], stdout=subprocess.PIPE, text=True)
    if result.returncode:
        print(text(result.stdout), file=sys.stderr)
        return result.returncode
    rows = json.loads(result.stdout)
    if not rows:
        print('No installed packages.')
        return 0
    with tempfile.TemporaryDirectory(prefix='haps-picker-') as directory:
        choices = choices_for(rows, directory)
        picked = subprocess.run(picker_args(action, False),
                                input='\n'.join(choices) + '\n', stdout=subprocess.PIPE, text=True,
                                env=dict(os.environ, HAPS_PICKER_DIR=directory))
        if picked.returncode:
            return 130 if picked.returncode in (1, 130) else picked.returncode
        selected = picked.stdout.rstrip('\n')
        if selected not in choices:
            raise ValueError('The package selection was not in the result list.')
        return subprocess.call(haps + [action, '--', choices[selected]['identity']])


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
