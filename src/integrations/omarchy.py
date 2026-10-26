"""Omarchy v4 shell plugin and the menu integration invoked by `haps omarchy`."""
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
if rows and not installed:
    print('Use a publisher/name above in “Install a package”. Haps checks trust before installing.')
'''

def menu_script(executable, home=None):
    command = [executable] + (['--home', home] if home is not None else [])
    search = ('show_packages() {\n  local results\n'
              '  [[ "$1" != search ]] || printf \'Finding packages…\\n\'\n'
              '  results=$("${haps[@]}" "$@") || return $?\n'
              '  printf \'%s\\n\' "$results" | python3 -c ' + shlex.quote(SEARCH_FORMATTER) + ' "$1"\n}\n')
    return '#!/usr/bin/env bash\nset -u\nhaps=(' + shlex.join(command) + ')\n' + search + r'''action=${1:-search}
if [[ "$action" == menu ]]; then
  selected=$(printf '%s\n' $'search\tFind packages' $'install\tInstall a package' $'list\tInstalled packages' $'update\tUpdate a package' $'remove\tRemove a package' |
    fzf --delimiter=$'\t' --with-nth=2 --prompt='Haps > ' --header='Packages from your social graph' --height=12 --layout=reverse) || exit 0
  action=${selected%%$'\t'*}
fi
case "$action" in
  search) read -r -p 'Find packages: ' package || exit 0; show_packages search --json -- "$package" ;;
  install) read -r -p 'Package (name or publisher/name): ' package || exit 0; [[ -n "$package" ]] || exit 0; "${haps[@]}" install -- "$package" ;;
  list) show_packages list --json ;;
  update|remove)
    packages=$("${haps[@]}" list)
    result=$?
    if [[ "$result" != 0 ]]; then
      printf '\nPress Enter to close.'; read -r _; exit "$result"
    elif [[ -z "$packages" ]]; then printf 'No installed packages.\n'
    else
      selected=$(printf '%s\n' "$packages" | fzf --delimiter=$'\t' --with-nth=1,2 --prompt="$action > ") || exit 0
      package=${selected%%$'\t'*}
      "${haps[@]}" "$action" -- "$package"
    fi ;;
  *) exit 2 ;;
esac
result=$?
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

def launch_plugin():
    command = shlex.join([sys.executable, str(Path(__file__).resolve()), '--menu'])
    return subprocess.call(['omarchy-launch-or-focus-tui', '--app-id=to.hashtree.haps', command])

def pause():
    try:
        input('\nPress Enter to close.')
    except (EOFError, KeyboardInterrupt):
        pass

def plugin_menu():
    executable = find_haps()
    if not executable:
        print('Haps — packages from your social graph.\n')
        print('Install the prebuilt Haps CLI and Hashtree helpers from haps.hashtree.cc.')
        print('The installer checks downloads and preserves existing commands. No sudo is used.\n')
        try:
            answer = input('Install Haps now? [y/N] ')
        except EOFError:
            return 0
        if answer.strip().lower() not in ('y', 'yes'):
            return 0
        installer = Path(__file__).resolve().parents[2] / 'website/public/install.sh'
        directory = install_directory()
        result = subprocess.call(['sh', str(installer), '--bin-dir', str(directory)])
        if result:
            pause()
            return result
        executable = str(directory / 'haps')
        if not os.access(executable, os.X_OK):
            raise ValueError('Installation did not create the Haps command. Check the installer output above.')
    if not shutil.which('fzf'):
        raise ValueError('The Haps menu needs fzf. Install it with: omarchy pkg add fzf')
    return subprocess.call(['bash', '-c', menu_script(executable), 'haps-menu', 'menu'])

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
    if any(k == 'haps' or k.startswith('haps.') for k in entries):
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
    rows = {'haps': {'label': 'Haps', 'icon': '󰏗', 'description': 'Packages from your social graph'}}
    for action, label in [('search', 'Find packages'), ('install', 'Install a package'), ('list', 'Installed packages'), ('update', 'Update a package'), ('remove', 'Remove a package')]:
        command = shlex.join([str(helper), action])
        rows['haps.' + action] = {'label': label, 'action': shlex.join(['omarchy-launch-or-focus-tui', '--app-id=to.hashtree.haps.' + action, command])}
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
    print('Haps menu removed.' if remove else 'Haps added to the Omarchy menu. Open the menu and choose Haps.')

if __name__ == '__main__':
    result = 0
    try:
        if sys.argv[1:] == ['--launch']:
            result = launch_plugin()
        elif sys.argv[1:] == ['--menu']:
            result = plugin_menu()
        elif len(sys.argv) >= 3:
            install(sys.argv[1], sys.argv[2], '--remove' in sys.argv[3:])
        else:
            raise ValueError('Usage: omarchy.py --launch | --menu | EXECUTABLE HOME [--remove]')
    except (OSError, ValueError) as error:
        print(str(error), file=sys.stderr)
        result = 1
        if sys.argv[1:] == ['--menu']:
            pause()
    except KeyboardInterrupt:
        result = 130
    sys.exit(result)
