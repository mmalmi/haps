import importlib.util
import json
import os
from pathlib import Path
import subprocess
import shlex
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('omarchy', Path(__file__).parents[1] / 'src/integrations/omarchy.py')
omarchy = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(omarchy)

class MenuTests(unittest.TestCase):
    def test_preserves_custom_menu_and_comments_and_is_idempotent(self):
        for raw in ['{\n  // Personal notes\n  "personal": {"label":"Notes"},\n}\n', '{\n"items": {\n"personal": {"label":"Notes"}\n}\n}\n']:
            rows = {'haps': {'label': 'Haps'}, 'haps.search': {'action': 'example'}}
            result = omarchy.merge(raw, rows)
            self.assertEqual(omarchy.merge(result, rows), result)
            self.assertEqual(omarchy.merge(result, {}), raw)
            self.assertIn('"personal": {"label":"Notes"}', result)
            parsed = omarchy.parse(result)[1]
            self.assertIn('haps', parsed.get('items', parsed))
        with self.assertRaises(ValueError):
            omarchy.merge('{"haps":{"label":"Mine"}}', {'haps': {}})
        with self.assertRaises(ValueError):
            omarchy.merge('{not json}', {'haps': {}})

    def test_install_run_update_and_remove_keep_user_configuration(self):
        with tempfile.TemporaryDirectory(prefix='haps menu $ ') as tmp:
            root = Path(tmp)
            menu = root / '.config/omarchy/extensions/omarchy-menu.jsonc'
            menu.parent.mkdir(parents=True)
            original = '{\n // keep me\n "personal": {"label":"Notes"}\n}\n'
            menu.write_text(original)
            executable = root / 'fake haps'
            executable.write_text('#!/usr/bin/env bash\nprintf "%s\\n" "$@" > "$HOME/args"\n')
            executable.chmod(0o755)
            home = root / 'data'
            with patch.dict(os.environ, {'HOME': str(root)}):
                omarchy.install(str(executable), str(home))
                first = menu.read_text()
                for action in ['search', 'install', 'list', 'update', 'remove']:
                    row = omarchy.parse(first)[1]['haps.' + action]
                    command = shlex.split(row['action'])
                    self.assertEqual(command[:2], ['omarchy-launch-or-focus-tui', '--app-id=to.hashtree.haps.' + action])
                    self.assertEqual(shlex.split(command[2]), [str(home / 'integrations/omarchy-menu'), action])
                omarchy.install(str(executable), str(home))
                self.assertEqual(first, menu.read_text())
                helper = home / 'integrations/omarchy-menu'
                subprocess.run(['bash', str(helper), 'install'], input='siriusbusiness/iris-chat\n\n', text=True, check=True)
                self.assertEqual((root / 'args').read_text().splitlines(), ['--home', str(home), 'install', '--', 'siriusbusiness/iris-chat'])
                omarchy.install(str(executable), str(home), remove=True)
                self.assertFalse(helper.exists())
                self.assertEqual(menu.read_text(), original)
                self.assertEqual(menu.with_suffix('.jsonc.haps-backup').read_text(), original)

if __name__ == '__main__':
    unittest.main()
