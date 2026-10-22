#!/usr/bin/env python3
"""Keep Haps <=0.1.17 able to audit an upgrade; edit only haps.toml."""
import argparse
from pathlib import Path

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--check', action='store_true', help='fail instead of updating the generated copy')
args = parser.parse_args()
canonical = (root / 'haps.toml').read_bytes()
generated = (b'# Generated compatibility copy for Haps <=0.1.17.\n'
             b'# Edit haps.toml, then run python3 scripts/sync-legacy-recipe.py.\n' + canonical)
path = root / 'haps-build.toml'
if args.check:
    if not path.exists() or path.read_bytes() != generated:
        parser.exit(1, 'Run python3 scripts/sync-legacy-recipe.py after editing haps.toml.\n')
else:
    path.write_bytes(generated)
