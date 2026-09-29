"""Public Python worker client: consistent evidence and no retry of writes."""
import copy
import importlib
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'python'))
from ddlog_runtime_client import ManagedWorldHost


class Client:
    def __init__(self):
        self.calls = []; self.revision = 4; self.generation = 2; self.rows = {}; self.reject_page = False
        self.publication = {'state':'durable','applied_revision':5,'receipt':{'exact':True},
                            'effect_authorized':True,'replayed':False}
    def request(self, operation, args):
        self.calls.append((operation, copy.deepcopy(args)))
        if operation == 'status':
            return {'ok':True,'result':{'id':'world','generation':self.generation,'revision':self.revision,'state':'running'}}
        if operation == 'read_batch':
            if self.reject_page and args['queries'][0]['continuation']:
                return {'ok':False,'error':'Expected revision mismatch; request not applied'}
            results = []
            for query in args['queries']:
                rows = self.rows.get(query['predicate'], [])
                start = query.get('continuation') or 0; end = start + query['max_rows']
                results.append({'revision':self.revision,'rows':rows[start:end],
                                'complete':end>=len(rows),'continuation':end if end<len(rows) else None})
            return {'ok':True,'result':{'id':'world','generation':2,'revision':self.revision,'results':results}}
        if operation == 'admit_inputs':
            return {'ok':True,'result':{'id':'world','generation':2,'admission_key':args['admission_key'],**self.publication}}
        raise AssertionError(operation)


class WorldClientTests(unittest.TestCase):
    def setUp(self):
        self.client = Client(); self.host = ManagedWorldHost(self.client,'world',2,worker_id='worker')

    def test_batches_and_pages_keep_one_evidence_revision(self):
        names = [f'relation{i}' for i in range(32)]
        self.client.rows[names[0]] = [[i] for i in range(70)]
        snapshot = self.host.snapshot(names)
        self.assertEqual(snapshot['relations'][names[0]], [[i] for i in range(70)])
        self.assertTrue(snapshot['complete'])
        batches = [args for op,args in self.client.calls if op == 'read_batch']
        self.assertEqual(len(batches), 3)
        for batch in batches:
            self.assertEqual((batch['expected_generation'],batch['expected_revision'],batch['worker_id']),(2,4,'worker'))
            self.assertLessEqual(sum(q['max_rows'] for q in batch['queries']),1000)

    def test_partial_changed_revision_and_limits_never_supply_prompt_evidence(self):
        self.client.rows['large'] = [[i] for i in range(70)]
        self.client.reject_page = True
        with self.assertRaisesRegex(RuntimeError,'revision mismatch'): self.host.snapshot(['large'])
        self.client.reject_page = False
        with self.assertRaisesRegex(RuntimeError,'row/byte bound'):
            ManagedWorldHost(self.client,'world',2,max_rows=10).snapshot(['large'])
        self.client.generation = 3
        with self.assertRaisesRegex(RuntimeError,'generation'): self.host.snapshot(['large'])

    def test_admission_is_one_request_with_explicit_effect_and_receipt(self):
        expected = {**self.host.identity,'revision':4}
        effect = {'key':'opaque','phase':'reserve'}
        reply = self.host.compare_apply(expected,[],{'admission_key':'request-1','effect':effect})
        self.assertEqual(self.client.calls,[('admit_inputs',{'id':'world','expected_generation':2,
            'worker_id':'worker','expected_revision':4,'changes':[],'admission_key':'request-1','effect':effect})])
        self.assertTrue(reply['effect_authorized'])
        self.assertEqual(reply['receipt'],{'exact':True})
        self.client.publication.update(state='uncertain',effect_authorized=False)
        reply = self.host.compare_apply(expected,[],{})
        self.assertIsNone(reply['applied'])
        self.assertFalse(reply['durable'])
        self.assertEqual(len(self.client.calls),2)

    def test_effect_authority_requires_fresh_durable_reservation(self):
        expected = {**self.host.identity, 'revision': 4}
        for phase, replayed, effect in [
            ('uncertain', False, {'phase':'reserve'}),
            ('applied_but_unpublished', False, {'phase':'reserve'}),
            ('not_applied', False, {'phase':'reserve'}),
            ('durable', True, {'phase':'reserve'}),
            ('durable', False, {'phase':'settle'}),
            ('durable', False, None),
        ]:
            with self.subTest(phase=phase, replayed=replayed, effect=effect):
                self.client.publication.update(state=phase, replayed=replayed, effect_authorized=True)
                reply = self.host.compare_apply(expected, [], {'effect':effect})
                self.assertFalse(reply['effect_authorized'])

    def test_mismatched_admission_reply_is_never_retried_or_authorized(self):
        expected = {**self.host.identity, 'revision':4}
        for key, value in [('id','other'),('generation',3),('admission_key','other'),
                           ('state','unexpected'),('receipt',None),('applied_revision',3)]:
            with self.subTest(key=key):
                client = Client()
                client.publication[key] = value
                host = ManagedWorldHost(client, 'world', 2)
                with self.assertRaisesRegex(RuntimeError, 'outcome unknown; no retry'):
                    host.compare_apply(expected, [], {'effect':{'phase':'reserve'}})
                self.assertEqual(len(client.calls), 1)

    def test_invalid_bounds_and_relations_are_rejected_before_requests(self):
        for value in [0, -1, float('nan'), True, 4*1024*1024+1]:
            with self.assertRaises(ValueError):
                ManagedWorldHost(self.client, 'world', 2, max_bytes=value)
        with self.assertRaises(ValueError): self.host.snapshot([{}])
        self.assertEqual(self.client.calls, [])


if __name__ == '__main__': unittest.main()
