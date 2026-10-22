"""One source manifest, reviewed local builds, and honest binary audit evidence."""
import json
import sys
import unittest

import audit_test


class SourceRecipes(unittest.TestCase):
    def setUp(self):
        self.fixture = audit_test.Audits('runTest')
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)

    def test_unified_manifest_rebuilds_and_verifies_published_binary(self):
        f = self.fixture
        f.publish(recipe_style='unified')
        f.run_cli('install', f.package, '--audit-agent', 'codex')
        evidence = json.loads(f.run_cli('info', 'hello', '--json').stdout)['audits'][0]['evidence']
        self.assertTrue(evidence['binary_match'])

    def test_source_install_reviews_source_but_need_not_match_published_binary(self):
        f = self.fixture
        f.publish(matches=False, recipe_style='unified')
        result = f.run_cli('install', f.package, '--from-source', '--audit-agent', 'codex', '--json')
        self.assertEqual(json.loads(result.stdout)['status'], 'installed')
        self.assertTrue((f.root / 'agent-called').exists())
        self.assertTrue((f.root / 'build-called').exists())
        receipt = next(iter(json.loads((f.home / 'installed.json').read_text()).values()))
        self.assertNotEqual(receipt['current']['pubkey'], f.author)
        self.assertIn('Reviewed source', result.stderr)

    def test_recipe_less_source_can_be_prepared_and_reviewed_without_approving_binary(self):
        f = self.fixture
        f.publish(recipe_style='none')
        f.run_cli('audit', 'prepare', 'hello')
        session = next((f.home / 'audits/pending').iterdir())
        result = f.run_cli('audit', 'finish', session.name, '--note', 'Reviewed source and instructions')
        self.assertIn('binary has not been verified', result.stdout)
        self.assertFalse((f.root / 'build-called').exists())
        info = json.loads(f.run_cli('info', 'hello', '--json').stdout)
        self.assertEqual(info['audits'], [])
        self.assertEqual(len(info['source_reviews']), 1)
        f.run_cli('install', f.package, success=False)

    def test_failed_source_review_never_builds_or_installs(self):
        f = self.fixture
        f.publish(recipe_style='unified')
        f.env['AUDIT_VERDICT'] = 'block'
        f.run_cli('install', f.package, '--from-source', '--audit-agent', 'codex', success=False)
        self.assertTrue((f.root / 'agent-called').exists())
        self.assertFalse((f.root / 'build-called').exists())
        self.assertFalse((f.home / 'installed.json').exists())

    def propose_documented_build(self, f):
        agent = f.bin / 'codex'
        proposal = {'verdict': 'ready', 'note': 'Use the documented build script.', 'files': [],
                    'build': {'builder': 'commands', 'commands': [[sys.executable, 'build.py']],
                              'artifacts': [{'destination': 'hello', 'path': 'hello'}]}}
        agent.write_text(agent.read_text().replace(
            'out.write_text(json.dumps(',
            "schema = json.loads(pathlib.Path(sys.argv[sys.argv.index('--output-schema') + 1]).read_text())\n"
            "if 'build' in schema['properties']:\n"
            f"    assert not pathlib.Path({str(f.root / 'build-called')!r}).exists()\n"
            f"    out.write_text({json.dumps(proposal)!r})\n    sys.exit(0)\n"
            'out.write_text(json.dumps('))

    def test_no_recipe_uses_project_instructions_then_reviews_before_building(self):
        f = self.fixture
        f.publish(matches=False, recipe_style='none')
        self.propose_documented_build(f)
        f.run_cli('install', f.package, '--from-source', '--audit-agent', 'codex')
        self.assertTrue((f.root / 'build-called').exists())
        receipts = json.loads((f.home / 'installed.json').read_text())
        identity = next(iter(receipts))
        evidence = json.loads(f.run_cli('info', identity, '--json').stdout)['audits'][0]['evidence']
        self.assertFalse(evidence['binary_match'])
        self.assertIn('recipe', evidence['provenance'])

    def test_agent_build_proposal_is_not_itself_an_audit_approval(self):
        f = self.fixture
        f.publish(recipe_style='none')
        self.propose_documented_build(f)
        f.env['AUDIT_VERDICT'] = 'block'
        f.run_cli('install', f.package, '--from-source', '--audit-agent', 'codex', success=False)
        self.assertTrue((f.root / 'agent-called').exists())
        self.assertFalse((f.root / 'build-called').exists())
        self.assertFalse((f.home / 'installed.json').exists())

    def test_unavailable_build_still_allows_source_review_but_not_binary_approval(self):
        f = self.fixture
        f.publish(recipe_style='unavailable')
        result = f.run_cli('install', f.package, '--from-source', '--audit-agent', 'codex', success=False)
        self.assertIn('Needs platform signing setup', result.stderr)
        self.assertFalse((f.root / 'agent-called').exists())
        f.run_cli('audit', 'prepare', 'hello')
        session = next((f.home / 'audits/pending').iterdir())
        result = f.run_cli('audit', 'finish', session.name, '--note', 'Reviewed the pinned source')
        self.assertIn('binary has not been verified', result.stdout)
        f.run_cli('install', f.package, success=False)
        self.assertFalse((f.root / 'build-called').exists())

    def test_local_recipe_is_audited_and_recorded_with_exact_source(self):
        f = self.fixture
        f.publish(matches=False, recipe_style='none')
        recipe = f.root / 'local-build.toml'
        recipe.write_text((f.source / 'haps.toml').read_text() +
                          '\n[build]\ncommands=[[' + json.dumps(sys.executable) +
                          ',"build.py"]]\n[build.artifacts]\nhello="hello"\n')
        f.run_cli('install', f.package, '--from-source', '--recipe-file', str(recipe), '--audit-agent', 'codex')
        receipt = next(iter(json.loads((f.home / 'installed.json').read_text()).values()))
        local = json.loads(receipt['current']['content'])['package']
        original = json.loads(f.release['content'])['package']
        self.assertEqual(local['source'], original['source'])
        self.assertIn('+haps.', local['version'])
        self.assertTrue((f.root / 'agent-called').exists())

    def test_direct_source_build_records_audited_recipe_in_local_catalog(self):
        f = self.fixture
        f.publish(recipe_style='unified')
        source = json.loads(f.release['content'])['package']['source']
        f.run_cli('build', source['git'], '--rev', source['rev'], '--package', 'hello',
                  '--execute', '--install', '--audit-agent', 'codex')
        identity = next(iter(json.loads((f.home / 'installed.json').read_text())))
        evidence = json.loads(f.run_cli('info', identity, '--json').stdout)['audits'][0]['evidence']
        self.assertFalse(evidence['binary_match'])
        self.assertIn('SHA-256', evidence['provenance'])

    def test_audit_installs_its_local_build_without_approving_mismatching_publisher_binary(self):
        f = self.fixture
        f.publish(matches=False, recipe_style='unified')
        f.run_cli('install', f.package, '--audit-agent', 'codex')
        receipt = next(iter(json.loads((f.home / 'installed.json').read_text()).values()))
        self.assertNotEqual(receipt['current']['pubkey'], f.author)
        info = json.loads(f.run_cli('info', f.package, '--json').stdout)
        self.assertEqual(info['audits'], [])
        build = info['source_reviews'][0]['evidence']['build']
        self.assertNotEqual(build['manifest'], json.loads(f.release['content'])['manifest'])
        self.assertEqual(len(build['recipe_sha256']), 64)

    def test_another_reader_builds_the_exact_recipe_from_a_trusted_source_review(self):
        f = self.fixture
        f.publish(matches=False, recipe_style='unified')
        f.run_cli('install', f.package, '--audit-agent', 'codex')
        info = json.loads(f.run_cli('info', f.package, '--json').stdout)
        event = info['source_reviews'][0]['event']
        shared = f.root / 'shared-audit.json'
        shared.write_text(json.dumps(event))
        reader = f.root / 'other-reader'
        f.run_cli('identity', 'init', home=reader)
        f.run_cli('follow', f.author, home=reader)
        f.run_cli('follow', event['pubkey'], home=reader)
        f.run_cli('source', 'add', 'publisher', str(f.root / 'repo'), '--author', f.author, home=reader)
        f.run_cli('import', str(shared), home=reader)
        (f.root / 'agent-called').unlink()
        (f.root / 'build-called').unlink()
        f.run_cli('install', f.package, home=reader)
        self.assertFalse((f.root / 'agent-called').exists())
        self.assertTrue((f.root / 'build-called').exists())
        receipt = next(iter(json.loads((reader / 'installed.json').read_text()).values()))
        self.assertNotEqual(receipt['current']['pubkey'], f.author)

    def test_trusted_source_review_does_not_authorize_a_changed_recipe(self):
        f = self.fixture
        f.publish(matches=False, recipe_style='unified')
        f.run_cli('install', f.package, '--audit-agent', 'codex')
        recipe = f.root / 'changed.toml'
        recipe.write_text((f.source / 'haps.toml').read_text().replace('"build.py"', '"different.py"'))
        (f.root / 'agent-called').unlink()
        (f.root / 'build-called').unlink()
        result = f.run_cli('install', f.package, '--from-source', '--recipe-file', str(recipe), success=False)
        self.assertIn('exact build instructions', result.stderr)
        self.assertFalse((f.root / 'agent-called').exists())
        self.assertFalse((f.root / 'build-called').exists())

    def test_mismatch_cannot_replace_required_independent_binary_audits(self):
        f = self.fixture
        f.publish(matches=False, recipe_style='unified')
        reviewer = f.root / 'independent-reviewer'
        key = f.run_cli('identity', 'init', home=reviewer).stdout.strip()
        f.run_cli('follow', key)
        claim = f.root / 'independent-audit.json'
        f.run_cli('attest', f.release['id'], '--audited', '--provenance', 'Reviewed fixture payload',
                  '--note', 'Independent fixture review', '--out', str(claim), home=reviewer)
        f.run_cli('import', str(claim))
        result = f.run_cli('install', f.package, '--require-attestations', '2', '--audit-agent', 'codex', success=False)
        self.assertIn('independent audit count', result.stderr)
        self.assertFalse((f.home / 'installed.json').exists())
        info = json.loads(f.run_cli('info', f.package, '--json').stdout)
        self.assertEqual(len(info['audits']), 1)
        self.assertEqual(len(info['source_reviews']), 1)

    def test_explicit_source_install_never_substitutes_an_audited_binary_without_source(self):
        f = self.fixture
        f.publish(recipe_style='unified')
        spec = f.root / 'haps.toml'
        spec.write_text(spec.read_text().split('[source]')[0].replace('1.0.0', '1.1.0'))
        release = json.loads(f.run_cli('pack', str(spec), '--payload', str(f.root / 'payload'),
                            '--out', str(f.root / 'repo'), home=f.publisher).stdout)
        f.run_cli('attest', release['id'], '--audited', '--provenance', 'Reviewed fixture bytes', '--note', 'Fixture')
        result = f.run_cli('install', f.package, '--from-source', success=False)
        self.assertIn('does not declare pinned source', result.stderr)
        self.assertFalse((f.home / 'installed.json').exists())

    def test_same_manifest_can_be_packed_without_a_separate_package_file(self):
        f = self.fixture
        f.publish(recipe_style='unified')
        result = f.run_cli('pack', str(f.source / 'haps.toml'), '--payload', str(f.root / 'payload'),
                          '--out', str(f.root / 'second-repo'), home=f.publisher)
        package = json.loads(json.loads(result.stdout)['content'])['package']
        self.assertEqual(package['name'], 'hello')
        self.assertNotIn('build', package)
        f.run_cli('add', str(f.source / 'haps.toml'), '--payload', str(f.root / 'payload'), home=f.publisher)


if __name__ == '__main__':
    unittest.main()
