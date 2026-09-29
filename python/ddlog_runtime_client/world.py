"""Translate generic owner contracts into a fenced, bounded worker access port."""
from copy import deepcopy
import json
import uuid


class ManagedWorldHost:
    def __init__(self, client, world_id, generation, *, worker_id=None, max_rows=10000, max_bytes=4*1024*1024):
        if not isinstance(world_id, str) or not world_id or type(generation) is not int or generation < 1:
            raise ValueError('A managed host requires an exact world/generation at startup')
        if type(max_rows) is not int or not 1 <= max_rows <= 10000:
            raise ValueError('max_rows must be within 1..10000')
        if type(max_bytes) is not int or not 1 <= max_bytes <= 4*1024*1024:
            raise ValueError('max_bytes must be within 1..4194304')
        self.client, self.worker_id = client, worker_id
        self.identity = {'world_id': world_id, 'generation': generation}
        self.max_rows, self.max_bytes = max_rows, max_bytes

    def _args(self, **values):
        return {'id':self.identity['world_id'], 'expected_generation':self.identity['generation'],
                **({'worker_id':self.worker_id} if self.worker_id else {}), **values}

    def _call(self, operation, args):
        response = self.client.request(operation, args)
        if not response.get('ok'):
            raise RuntimeError(response.get('error') or 'Runtime rejected the request')
        return response['result']

    def snapshot(self, relations):
        if (not isinstance(relations, list) or len(relations) > 128
                or any(not isinstance(name, str) or not name for name in relations)
                or len(set(relations)) != len(relations)):
            raise ValueError('Snapshot requires at most 128 unique relation names')
        status = self._call('status', {'id':self.identity['world_id']})
        revision = status.get('revision')
        if (status.get('id') != self.identity['world_id']
                or status.get('generation') != self.identity['generation'] or status.get('state') != 'running'
                or type(revision) is not int or revision < 0):
            raise RuntimeError('Worker world/generation is no longer running')
        result, total_rows, total_bytes = {}, 0, 0
        # Bound each batch to the owner's 1000-row limit. The same revision is
        # mandatory across every page and batch, so a concurrent write rejects
        # the whole observation rather than mixing revisions.
        pending = [(name, None) for name in relations]
        result = {name: [] for name in relations}
        while pending:
            selected, pending = pending[:16], pending[16:]
            page_size = min(62, self.max_rows - total_rows + 1)
            queries = [{'predicate':name, 'max_rows':page_size, 'continuation':continuation}
                       for name, continuation in selected]
            batch = self._call('read_batch', self._args(expected_revision=revision, queries=queries))
            if (batch.get('generation') != self.identity['generation'] or batch.get('revision') != revision
                    or batch.get('id') != self.identity['world_id'] or len(batch.get('results', [])) != len(selected)):
                raise RuntimeError('Inconsistent snapshot batch identity')
            for (name, continuation), page in zip(selected, batch['results']):
                if page.get('revision') != revision or not isinstance(page.get('rows'), list):
                    raise RuntimeError('Inconsistent snapshot relation revision')
                total_bytes += len(json.dumps(page['rows']).encode())
                total_rows += len(page['rows'])
                if total_rows > self.max_rows or total_bytes > self.max_bytes:
                    raise RuntimeError('Snapshot exceeds worker row/byte bound; no partial evidence returned')
                result[name].extend(page['rows'])
                if page.get('complete') is True: continue
                next_page = page.get('continuation')
                if next_page is None or next_page == continuation:
                    raise RuntimeError('Incomplete snapshot page has no advancing continuation')
                pending.append((name, next_page))
        return {**self.identity, 'revision':revision, 'complete':True, 'relations':result}

    def rows(self, name):
        return self.snapshot([name])['relations'][name]

    def compare_apply(self, expected, changes, evidence):
        if (any(expected.get(k) != v for k,v in self.identity.items())
                or type(expected.get('revision')) is not int or expected['revision'] < 0):
            raise ValueError('Mutation evidence does not match the worker identity/revision')
        evidence = deepcopy(evidence)
        args = self._args(expected_revision=expected['revision'], changes=changes,
                          admission_key=evidence.get('admission_key') or uuid.uuid4().hex)
        if evidence.get('effect') is not None: args['effect'] = evidence['effect']
        publication = self._call('admit_inputs', args)
        phase = publication.get('state')
        if (publication.get('id') != self.identity['world_id']
                or publication.get('generation') != self.identity['generation']
                or publication.get('admission_key') != args['admission_key']
                or phase not in ('not_applied', 'durable', 'applied_but_unpublished', 'uncertain')):
            raise RuntimeError('Inconsistent admission identity or state; outcome unknown; no retry')
        durable = phase == 'durable'
        if durable and (not isinstance(publication.get('receipt'), dict)
                        or type(publication.get('applied_revision')) is not int
                        or publication['applied_revision'] < expected['revision']):
            raise RuntimeError('Invalid durable admission boundary; outcome unknown; no retry')
        authority = (durable and publication.get('effect_authorized') is True
                     and publication.get('replayed') is False
                     and (args.get('effect') or {}).get('phase') == 'reserve')
        return {**self.identity, 'applied': None if phase == 'uncertain' else phase in ('durable','applied_but_unpublished'),
                'durable':durable, 'revision':publication.get('applied_revision'),
                'receipt':publication.get('receipt'), 'error':publication.get('error'),
                'admission_key':publication.get('admission_key'),
                'effect_authorized':authority,
                'replayed':publication.get('replayed'), 'publication':publication}
