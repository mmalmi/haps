import argparse
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('index_worker', ROOT / 'scripts/index-worker.py')
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)


class IndexWorkerTest(unittest.TestCase):
    def test_unchanged_index_skips_upload_and_failed_upload_retries(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home, output = root / 'private', root / 'public'
            home.mkdir()
            (home / 'identity.key').write_text('fixture key, never used for signing')
            output.mkdir()
            event = {'id': 'abc', 'content': json.dumps({'root': 'fixture-root'})}
            (output / 'index.json').write_text(json.dumps(event))
            haps = root / 'haps'
            haps.write_text('#!/bin/sh\nexit 0\n')
            haps.chmod(0o755)
            htree = root / 'htree'
            htree.write_text('#!/bin/sh\nif [ -e "$WORKER_FAIL" ]; then exit 1; fi\nprintf done >> "$WORKER_UPLOADS"\n')
            htree.chmod(0o755)
            uploads, fail = root / 'uploads', root / 'fail'
            args = argparse.Namespace(home=home, output=output, haps=str(haps), htree=str(htree), name='packages', timeout=10, max_state_mib=1, min_free_gib=0)
            with patch.dict(os.environ, WORKER_UPLOADS=str(uploads), WORKER_FAIL=str(fail)):
                fail.touch()
                with self.assertRaisesRegex(RuntimeError, 'exited with 1'):
                    worker.run(args)
                self.assertFalse((home / 'index-published.json').exists())
                fail.unlink()
                worker.run(args)
                worker.run(args)
                self.assertEqual(uploads.read_text(), 'done')
                args.min_free_gib = 10**12
                with self.assertRaisesRegex(RuntimeError, 'headroom'):
                    worker.run(args)
                self.assertEqual(uploads.read_text(), 'done')
                args.min_free_gib = 0
                event['content'] = json.dumps({'root': None})
                (output / 'index.json').write_text(json.dumps(event))
                with self.assertRaisesRegex(RuntimeError, 'empty publication'):
                    worker.run(args)
                self.assertEqual(uploads.read_text(), 'done')


if __name__ == '__main__':
    unittest.main()
