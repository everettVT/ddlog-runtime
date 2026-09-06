"""MCP interface/admission contracts with a SIMULATED runtime, not DDlog semantics."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('composition_test_driver', ROOT / 'scripts/test-shared-instance.py')
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)

DEFINITION = {'rules': 'scratch(X) :- source(X). result(X) :- scratch(X).', 'schemas': {
    'source': {'input': True, 'fields': ['string']},
    'scratch': {'input': False, 'fields': ['string']},
    'result': {'input': False, 'fields': ['string']}},
    'interface': {'inputs': ['source'], 'outputs': ['result']}}

class CompositionAdmission(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='lmcompose-', dir='/tmp')
        self.root = Path(self.tmp.name)
        control = self.root / 'control'
        control.mkdir()
        env = dict(os.environ, FAKE_CONTROL=str(control),
                   LEMMALOG_PROCESSOR_REGISTRY=str(self.root / 'registry'),
                   LEMMALOG_DDLOG_BUILD=str(ROOT / 'tests/fixtures/shared_fake_build.py'))
        env.pop('LEMMALOG_AGENT_OPERATIONS', None)
        self.host = driver.Host(self.root, 1, env)
        self.client = self.host.client()
        self.client.initialize()
        self.leaf = self.client.call('processor_create', {'definition': DEFINITION})
        ref = {k: self.leaf[k] for k in ('processor_id', 'version')}
        self.definition = {'composition': {'nodes': {'first': ref, 'second': ref},
            'inputs': {'source': {'fields': ['string'], 'targets': [{'node': 'first', 'relation': 'source'}]}},
            'bindings': [{'from': {'node': 'first', 'relation': 'result'}, 'to': {'node': 'second', 'relation': 'source'}}],
            'outputs': {'result': {'node': 'second', 'relation': 'result'}}}}

    def tearDown(self):
        try:
            self.host.stop()
        finally:
            if self.client.process.poll() is None:
                self.client.close()
            self.tmp.cleanup()

    def error(self, name, args):
        return self.client.call(name, args, error=True)['content'][0]['text']

    def test_validation_errors_name_endpoints_and_do_not_publish(self):
        composition = self.client.call('processor_create', {'definition': self.definition})
        cases = []
        def changed():
            return json.loads(json.dumps(self.definition))
        invalid = changed()
        invalid['composition']['bindings'] = []
        cases.append((invalid, 'Unconnected input second.source'))
        invalid = changed()
        invalid['composition']['inputs']['source']['fields'] = ['int']
        cases.append((invalid, 'Binding type mismatch: input source has ["int"], first.source requires ["string"]'))
        invalid = changed()
        invalid['composition']['bindings'][0]['from']['relation'] = 'scratch'
        cases.append((invalid, 'first.scratch is not an exported output'))
        invalid = changed()
        invalid['composition']['outputs']['source'] = {'node': 'second', 'relation': 'result'}
        cases.append((invalid, 'Ambiguous external name source'))
        invalid = changed()
        invalid['composition']['bindings'][0]['from']['node'] = 'missing'
        cases.append((invalid, 'Unknown node missing'))
        invalid = changed()
        invalid['composition']['bindings'].append(invalid['composition']['bindings'][0])
        cases.append((invalid, 'Multiple sources for input second.source'))
        invalid = changed()
        invalid['composition']['inputs'] = {}
        invalid['composition']['bindings'].append({'from': {'node': 'second', 'relation': 'result'}, 'to': {'node': 'first', 'relation': 'source'}})
        cases.append((invalid, 'Recursive rules'))
        for definition, expected in cases:
            error = self.error('processor_publish', {'processor_id': composition['processor_id'], 'expected_version': composition['version'], 'definition': definition})
            self.assertIn(expected, error)
            self.assertIn("correct", error.lower())
            self.assertEqual(self.client.call('processor_get', {'processor_id': composition['processor_id']})['version'], composition['version'])
        self.assertEqual(self.client.call('instance_info')['health'], 'uninitialized')
        self.assertFalse(list(self.root.rglob('program.dl')), 'Saving must not invoke the compiler')

    def test_authoring_errors_preserve_cause_and_offer_correction(self):
        missing = json.loads(json.dumps(self.definition))
        del missing['composition']['nodes']['first']['version']
        error = self.error('processor_create', {'definition': missing})
        self.assertIn('version', error)
        self.assertIn('missing', error.lower())
        self.assertIn('correct', error.lower())
        bad = json.loads(json.dumps(DEFINITION))
        bad['rules'] = 'result(X) :- source(X, X).'
        error = self.error('processor_create', {'definition': bad})
        for fragment in ('Arity', 'source', 'expected', 'got', 'Next action'):
            self.assertIn(fragment, error)
        unknown = self.error('processor_get', {'processor_id': 'processor_' + '0' * 32})
        self.assertIn('processor_', unknown)
        self.assertIn('processor_list', unknown)
        self.assertIn('processor_search', unknown)
        self.assertEqual(self.client.call('instance_info')['health'], 'uninitialized')

    def test_manifest_install_pins_and_enforces_public_ports(self):
        record = self.client.call('processor_create', {'definition': self.definition})
        installed = self.client.call('processor_install', {k: record[k] for k in ('processor_id', 'version')})
        self.assertEqual(installed['composition'], record['composition'])
        self.assertEqual(self.client.call('instance_info')['composition'], record['composition'])
        self.assertIn('Unknown exported output scratch', self.error('lemmalog_query', {'predicate': 'scratch'}))
        physical = record['composition']['outputs']['result']
        self.assertIn('Unknown exported output', self.error('lemmalog_query', {'predicate': physical}))
        self.assertIn('Unknown exported input', self.error('apply_changes', {'changes': [{'op': 'insert', 'predicate': 'Module0_source', 'values': ['x']}]}))
        self.client.call('apply_changes', {'changes': [{'op': 'insert', 'predicate': 'source', 'values': ['x']}]})
        # Fake runtime returns empty rows; this asserts routing only.
        self.assertEqual(self.client.call('lemmalog_query', {'predicate': 'result'})['rows'], '')
        witness = self.client.call('lemmalog_why', {'rule': 0})
        self.assertEqual(witness['origin'], record['composition']['rules'][0])
        self.assertIn('pinned', self.error('processor_install', {k: self.leaf[k] for k in ('processor_id', 'version')}))

    def test_discovery_and_archive_preserve_exact_composition_references(self):
        composition = self.client.call('processor_create', {'definition': self.definition})
        first = self.client.call('processor_list', {'limit': 1})
        second = self.client.call('processor_list', {'limit': 1, 'after': first['next_cursor']})
        identities = [first['processors'][0]['processor_id'], second['processors'][0]['processor_id']]
        self.assertEqual(identities, sorted([self.leaf['processor_id'], composition['processor_id']]))
        self.assertTrue(all(row['status'] == 'active' for page in (first, second) for row in page['processors']))
        found = self.client.call('processor_search', {'query': 'SCRATCH'})
        self.assertEqual([row['processor_id'] for row in found['processors']], [self.leaf['processor_id']])
        archived = self.client.call('processor_archive', {'processor_id': self.leaf['processor_id'], 'expected_version': self.leaf['version'], 'expected_revision': 0})
        self.assertEqual(archived['status'], 'archived')
        self.assertEqual(archived['lifecycle_revision'], 1)
        self.assertEqual(archived, self.client.call('processor_archive', {'processor_id': self.leaf['processor_id'], 'expected_version': self.leaf['version'], 'expected_revision': 1}))
        stale = self.error('processor_archive', {'processor_id': self.leaf['processor_id'], 'expected_version': self.leaf['version'], 'expected_revision': 0})
        self.assertIn('conflict', stale.lower())
        self.assertIn('read', stale.lower())
        self.assertIn('conflict', self.error('processor_archive', {'processor_id': self.leaf['processor_id'], 'expected_version': composition['version'], 'expected_revision': 1}).lower())
        self.assertEqual([row['processor_id'] for row in self.client.call('processor_list')['processors']], [composition['processor_id']])
        self.assertEqual(self.client.call('processor_search', {'query': 'scratch'})['processors'], [])
        included = self.client.call('processor_search', {'query': 'scratch', 'include_archived': True})['processors']
        self.assertEqual(included[0]['status'], 'archived')
        self.assertEqual(len(self.client.call('processor_list', {'include_archived': True})['processors']), 2)
        exact = {k: self.leaf[k] for k in ('processor_id', 'version')}
        self.assertEqual(self.client.call('processor_get', exact), self.leaf)
        self.assertIn('archiv', self.error('processor_get', {'processor_id': self.leaf['processor_id']}).lower())
        self.assertIn('archiv', self.error('processor_install', exact).lower())
        self.assertEqual(self.client.call('processor_get', {'processor_id': composition['processor_id']}), composition)
        # An existing composition remains installable through retained exact refs.
        self.client.call('processor_install', {k: composition[k] for k in ('processor_id', 'version')})
        before = self.client.call('instance_info')
        self.assertEqual(before['health'], 'ready')
        restored = self.client.call('processor_restore', {'processor_id': self.leaf['processor_id'], 'expected_version': self.leaf['version'], 'expected_revision': 1})
        self.assertEqual(restored['status'], 'active')
        self.assertEqual(restored['lifecycle_revision'], 2)
        self.assertEqual(restored, self.client.call('processor_restore', {'processor_id': self.leaf['processor_id'], 'expected_version': self.leaf['version'], 'expected_revision': 2}))
        self.assertEqual(self.client.call('processor_get', {'processor_id': self.leaf['processor_id']}), self.leaf)
        self.assertEqual(self.client.call('instance_info'), before)
        active = self.client.call('processor_search', {'query': 'scratch'})['processors']
        self.assertEqual(active[0]['status'], 'active')
        self.assertEqual(active[0]['lifecycle_revision'], 2)
        self.client.call('processor_create', {'definition': self.definition})
        for tool in ('processor_archive', 'processor_restore'):
            error = self.error(tool, {'processor_id': self.leaf['processor_id'], 'expected_version': self.leaf['version'], 'expected_revision': 0})
            self.assertIn('conflict', error.lower())
            self.assertIn('read', error.lower())

    def test_composed_program_is_usable_through_the_same_ports_and_lifecycle(self):
        inner = self.client.call('processor_create', {'definition': self.definition})
        wrapper = {'composition': {'nodes': {'nested': {k: inner[k] for k in ('processor_id', 'version')}},
            'inputs': {'source': {'fields': ['string'], 'targets': [{'node': 'nested', 'relation': 'source'}]}},
            'bindings': [], 'outputs': {'result': {'node': 'nested', 'relation': 'result'}}}}
        outer = self.client.call('processor_create', {'definition': wrapper})
        self.assertEqual(len(outer['composition']['dependencies']), 3)
        self.assertEqual(outer['composition']['dependencies']['nested.first']['version'], self.leaf['version'])
        self.assertTrue(all(row['kind'] == 'program' for row in self.client.call('processor_list')['processors']))
        self.client.call('processor_install', {k: outer[k] for k in ('processor_id', 'version')})
        self.client.call('apply_changes', {'changes': [{'op': 'insert', 'predicate': 'source', 'values': ['x']}]})
        self.assertEqual(self.client.call('lemmalog_query', {'predicate': 'result'})['rows'], '')
        why = self.client.call('lemmalog_why', {'rule': 0})
        self.assertEqual(why['origin']['node'], 'nested.first')
        self.assertEqual(why['origin']['version'], self.leaf['version'])
        self.assertIn('Unknown exported output', self.error('lemmalog_query', {'predicate': 'nested.result'}))

    def test_individual_interface_hides_private_relation(self):
        self.client.call('processor_install', {k: self.leaf[k] for k in ('processor_id', 'version')})
        self.assertIn('Unknown exported output scratch', self.error('lemmalog_query', {'predicate': 'scratch'}))
        self.assertEqual(self.client.call('lemmalog_query', {'predicate': 'result'})['rows'], '')

if __name__ == '__main__':
    unittest.main()
