#!/usr/bin/env python3
"""Real DDlog/MCP Large-Star/Small-Star acceptance; no simulated runtime.

Requires LEMMALOG_DDLOG_MCP, LEMMALOG_DDLOG_BUILD and the supported DDlog
operator build environment. STAR_ARTIFACTS retains raw receipts and generated
source/executable/build logs. Uses one native compilation in a fresh run.
"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import random
import shutil
import sys
import tempfile
import traceback

from connected_components_oracle import apply, expected, rows, trace

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('shared', HERE / 'test-shared-instance.py')
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
ARTIFACTS = Path(os.environ.get('STAR_ARTIFACTS') or tempfile.mkdtemp(prefix='star-proof-'))
ARTIFACTS.mkdir(parents=True, exist_ok=True)
receipt = {'passed': False, 'backend': 'official DDlog / Differential Dataflow',
           'algorithm': 'alternating_large_star_small_star',
           'server_sha256': shared.BINARY_SHA256, 'checks': [], 'snapshots': [], 'rpc': []}


def call(client, name, arguments=None):
    result = client.call(name, arguments)
    receipt['rpc'].append({'tool': name, 'arguments': arguments or {}, 'result': result})
    return result


def ref(record):
    return {key: record[key] for key in ('processor_id', 'version')}


def observe(client, name, facts):
    result = call(client, 'lemmalog_query', {'predicate': 'labels'})
    actual = rows(result['rows'])
    oracle = expected(facts)
    assert actual == oracle, {'phase': name, 'actual': sorted(actual), 'expected': sorted(oracle)}
    receipt['snapshots'].append({'phase': name, 'expected': sorted(oracle), 'actual': sorted(actual)})
    return result


def run(root):
    env = dict(os.environ, LEMMALOG_PROCESSOR_REGISTRY=str(root / 'registry'))
    host = shared.Host(root, 1, env)
    author, reviewer = host.client(), host.client()
    assert author.process.pid != reviewer.process.pid
    definition = {'rules': 'labels(V,C) :- components(V,C).', 'schemas': {
        'vertices': {'input': True, 'fields': ['int']},
        'edges': {'input': True, 'fields': ['int', 'int']},
        'components': {'input': False, 'fields': ['int', 'int']},
        'labels': {'input': False, 'fields': ['int', 'int']}},
        'operators': [{'type': 'large_small_star', 'vertices': 'vertices',
                       'edges': 'edges', 'output': 'components'}],
        'interface': {'inputs': ['vertices', 'edges'], 'outputs': ['labels']}}
    leaf = call(author, 'processor_create', {'definition': definition})
    assert leaf['validation']['types_checked']
    assert not leaf['validation']['ddlog_compilation_performed']
    wrapper = {'composition': {
        'nodes': {'graph': ref(leaf)},
        'inputs': {name: {'fields': fields, 'targets': [{'node': 'graph', 'relation': name}]}
                   for name, fields in [('vertices', ['int']), ('edges', ['int', 'int'])]},
        'bindings': [], 'outputs': {'labels': {'node': 'graph', 'relation': 'labels'}}}}
    registered = call(author, 'processor_create', {'definition': wrapper})
    installed = call(author, 'processor_install', ref(registered))
    assert installed['processor'] == ref(registered)
    assert call(reviewer, 'instance_info')['processor'] == ref(registered)
    receipt['processor'] = ref(registered)
    receipt['leaf_processor'] = ref(leaf)
    receipt['checks'].append('registered_and_composed_ordinary_program_with_pinned_native_operator')
    facts = {'vertices': set(), 'edges': set()}
    observe(reviewer, 'empty', facts)
    for index, phase in enumerate(trace()):
        client = author if index % 2 == 0 else reviewer
        result = call(client, 'apply_changes', {'changes': phase['changes']})
        facts = apply(facts, phase['changes'])
        observe(reviewer if client is author else author, phase['name'], facts)
        if phase['name'] in ('insert_bridge', 'delete_last_bridge'):
            assert ': -1' in result['deltas'] and ': +1' in result['deltas'], result
    receipt['checks'].append('bridge_merge_split_isolates_signed_ids_duplicate_reverse_support_and_endpoint_removal')
    # Removing a component's minimum must increase surviving labels; it cannot
    # reuse connectivity encoded only in the old contracted star.
    changes = [{'op': 'delete', 'predicate': 'vertices', 'values': [-9]}]
    changes += [{'op': 'delete', 'predicate': 'edges', 'values': list(edge)}
                for edge in sorted(facts['edges']) if -9 in edge]
    call(author, 'apply_changes', {'changes': changes})
    facts = apply(facts, changes)
    observe(reviewer, 'remove_minimum_and_incident_edges', facts)
    author.close()
    author = host.client()
    observe(author, 'author_reconnected', facts)
    receipt['checks'].append('independent_pipe_clients_disconnect_reconnect_retains_instance')

    # Relabelled path forces contraction across neighborhoods rather than
    # accepting stars already present in the input. Then deterministic updates
    # stress deletions after previous fixed points against fresh BFS partitions.
    changes = [{'op': 'delete', 'predicate': name, 'values': list(value)}
               for name in facts for value in sorted(facts[name])]
    vertices = [17, -100, 48, 0, 13, -2, 55, 4, 60, -7, 72, 6, 99]
    changes += [{'op': 'insert', 'predicate': 'vertices', 'values': [v]} for v in vertices]
    changes += [{'op': 'insert', 'predicate': 'edges', 'values': [u, v]}
                for u, v in zip(vertices, vertices[1:])]
    call(author, 'apply_changes', {'changes': changes})
    facts = apply(facts, changes)
    observe(reviewer, 'relabeled_path', facts)
    rng = random.Random(2014)
    for index in range(80):
        u, v = rng.sample(vertices, 2)
        # Delete an existing edge often enough to exercise actual retractions.
        if index % 3 == 0 and facts['edges']:
            u, v = rng.choice(sorted(facts['edges']))
            op = 'delete'
        else:
            op = 'insert' if index % 3 else 'delete'
        changes = [{'op': op, 'predicate': 'edges', 'values': [u, v]}]
        call(reviewer, 'apply_changes', {'changes': changes})
        facts = apply(facts, changes)
        observe(author, f'deterministic_mutation_{index}', facts)
    changes = [{'op': 'delete', 'predicate': name, 'values': list(value)}
               for name in facts for value in sorted(facts[name])]
    call(author, 'apply_changes', {'changes': changes})
    facts = apply(facts, changes)
    observe(reviewer, 'complete_retraction', facts)
    receipt['checks'].append('relabeled_path_80_mutations_and_complete_retraction_match_independent_bfs')
    author.close()
    reviewer.close()
    host.stop()
    assert not host.descriptor.exists()
    receipt['checks'].append('explicit_stop_and_bounded_cleanup')
    sources = list((host.directory / 'build').rglob('program.dl'))
    assert len(sources) == 1, f'Expected one generated program, found {sources}'
    build = sources[0].parent
    retained = ARTIFACTS / 'native-build'
    retained.mkdir(exist_ok=True)
    receipt['build'] = {}
    for filename in ['program.dl', 'lemmalog_star.dl', 'lemmalog_star.rs', 'program_cli', 'build.log']:
        source = build / filename
        shutil.copyfile(source, retained / filename)
        receipt['build'][filename] = {'sha256': hashlib.sha256(source.read_bytes()).hexdigest(),
                                     'bytes': source.stat().st_size}
    assert installed['composition']['generated_source_sha256'] == receipt['build']['program.dl']['sha256']
    source = (retained / 'program.dl').read_text()
    assert 'apply lemmalog_star::LargeSmallStar' in source
    for filename in ['lemmalog_star.dl', 'lemmalog_star.rs']:
        assert receipt['build'][filename]['sha256'] in source
    receipt['generated_source'] = source
    receipt['native_library'] = (retained / 'lemmalog_star.rs').read_text()
    receipt['native_activations'] = 1
    receipt['artifact_mode'] = os.environ.get('NATIVE_ARTIFACT_MODE', 'compile')
    assert receipt['artifact_mode'] in ('compile', 'verified-reuse')
    receipt['native_builds'] = int(receipt['artifact_mode'] == 'compile')


if __name__ == '__main__':
    root = Path(tempfile.mkdtemp(prefix='lmstar-', dir='/tmp'))
    try:
        run(root)
        assert hashlib.sha256(Path(shared.BINARY).read_bytes()).hexdigest() == shared.BINARY_SHA256
        receipt['passed'] = True
    except BaseException:
        receipt['error'] = traceback.format_exc()
        # Leave source, build logs and fixtures reviewable even after cleanup.
        for source in root.rglob('build.log'):
            print(source.read_text(errors='replace')[-10000:], file=sys.stderr)
        raise
    finally:
        errors = []
        for host in shared.hosts:
            if not host.log.closed:
                try:
                    host.stop()
                except Exception as error:
                    errors.append(str(error))
        for client in shared.clients:
            if not client.errors.closed:
                try:
                    client.close()
                except Exception as error:
                    errors.append(str(error))
        receipt['cleanup_errors'] = errors
        receipt['passed'] = receipt['passed'] and not errors
        receipt['raw_directory'] = str(root)
        (ARTIFACTS / 'real-backend-receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
        print(json.dumps({'passed': receipt['passed'], 'checks': receipt['checks'],
                          'snapshots': len(receipt['snapshots']), 'receipt': str(ARTIFACTS / 'real-backend-receipt.json')}))
