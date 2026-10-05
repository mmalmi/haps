"""Small Omarchy v4 menu integration, invoked by `haps omarchy`."""
import json
import os
from pathlib import Path
import re
import shlex
import sys
import tempfile

START = '  // BEGIN HAPS MENU\n'
END = '  // END HAPS MENU\n'

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
        rows['haps.' + action] = {'label': label, 'action': shlex.join(['omarchy-launch-or-focus-tui', command])}
    changed = merge(raw, {} if remove else rows)
    backup = menu.with_suffix('.jsonc.haps-backup')
    if menu.exists() and not backup.exists():
        # Preserve the exact original once. Never overwrite a prior backup.
        with backup.open('x') as file:
            file.write(raw)
    if not remove:
        script = '#!/usr/bin/env bash\nset -u\nhaps=(' + shlex.join([executable, '--home', home]) + ')\n' + '''action=${1:-search}
case "$action" in
  search) read -r -p 'Find packages: ' package; "${haps[@]}" search -- "$package" ;;
  install) read -r -p 'Package (name or publisher/name): ' package; [[ -z "$package" ]] || "${haps[@]}" install -- "$package" ;;
  list) "${haps[@]}" list ;;
  update|remove)
    packages=$("${haps[@]}" list) || exit $?
    if [[ -z "$packages" ]]; then printf 'No installed packages.\\n'
    else
      selected=$(printf '%s\\n' "$packages" | fzf --delimiter=$'\\t' --with-nth=1,2 --prompt="$action > ") || exit 0
      package=${selected%%$'\\t'*}
      "${haps[@]}" "$action" -- "$package"
    fi ;;
  *) exit 2 ;;
esac
result=$?
printf '\\n'
read -r -p 'Press Enter to close.' _
exit "$result"
'''
        atomic(helper, script, 0o755)
    atomic(menu, changed, menu.stat().st_mode & 0o777 if menu.exists() else 0o644)
    if remove:
        helper.unlink(missing_ok=True)
    print('Haps menu removed.' if remove else 'Haps added to the Omarchy menu. Open the menu and choose Haps.')

if __name__ == '__main__':
    try:
        install(sys.argv[1], sys.argv[2], '--remove' in sys.argv[3:])
    except (OSError, ValueError) as error:
        sys.exit(str(error))
