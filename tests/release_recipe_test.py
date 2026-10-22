"""The source recipe must describe the same package as each published bundle."""
import json
import subprocess
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
try:
    import tomllib
except ModuleNotFoundError:
    tomllib = None

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import bundle


class ReleaseRecipeTest(unittest.TestCase):
    def test_legacy_self_upgrade_recipe_is_an_exact_generated_copy(self):
        subprocess.run([sys.executable, str(ROOT / 'scripts/sync-legacy-recipe.py'), '--check'], check=True)

    @unittest.skipUnless(tomllib, 'Recipe metadata validation needs Python 3.11+')
    def test_recipe_matches_every_published_platform(self):
        recipe = tomllib.loads((ROOT / 'haps.toml').read_text())
        config = json.loads((ROOT / 'haps-release.json').read_text())
        version = tomllib.loads((ROOT / 'Cargo.toml').read_text())['package']['version']
        for platform in config['packages']:
            target = platform['target']
            suffix = '.exe' if 'windows' in target else ''
            expand = lambda text: text.replace('{target}', target).replace('{exe}', suffix)
            package = dict(recipe['package'], target=target)
            package['commands'] = {k: expand(v) for k, v in package['commands'].items()}
            expected = {k: v for k, v in platform.items() if k != 'asset'}
            self.assertEqual(package, dict(expected, version=version), target)
            artifacts = {expand(k) for k in recipe['build']['artifacts']}
            self.assertEqual(artifacts, set(package['commands'].values()) |
                             {'bundle.json', 'libexec/hashtree-LICENSE'}, target)

    def test_release_and_audit_use_identical_bundle_metadata(self):
        metadata = {'hashtree': 'v0.2.150', 'archive_sha256': 'a' * 64}
        with tempfile.TemporaryDirectory() as temp, patch.object(bundle, 'stage_helpers', return_value=metadata) as stage:
            root = Path(temp)
            bundle.stage_bundle('aarch64-apple-darwin', root, root / 'cache')
            stage.assert_called_once_with('aarch64-apple-darwin', root / 'libexec', root / 'cache')
            self.assertEqual((root / 'bundle.json').read_text(), json.dumps(metadata, indent=2) + '\n')


if __name__ == '__main__':
    unittest.main()
