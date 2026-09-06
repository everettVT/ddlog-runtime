#!/usr/bin/env python3
"""Five native graph activations with nested programs and an independent set oracle.

No provider calls, simulated graph evaluation, downloads, or automatic retries.
The three individual processors, two nested program versions, reviewer bridge,
and registry lifecycle assertions all use the actual Rust MCP host and DDlog.
"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import tempfile

from composition_oracle import apply, expected, rows

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('shared_driver', ROOT / 'scripts/test-shared-instance.py')
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)
REPORT = Path(os.environ.get('COMPOSITION_RECEIPT', '/tmp/composition-receipt.json'))
ARTIFACTS = Path(os.environ.get('COMPOSITION_ARTIFACTS', '/tmp/composition-artifacts'))
FIXTURE = json.loads((ROOT / 'docs/evidence/composition/fixture.json').read_text())
EXPECTED_SERVER_SHA256 = os.environ.get('COMPOSITION_EXPECT_SERVER_SHA256', driver.BINARY_SHA256)
checks, builds, snapshots, activations = [], [], [], []
processors = {}


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def toolchain_identity():
    return {
        'server_sha256': sha256(driver.BINARY),
        'compiler_sha256': sha256(Path(os.environ['DDLOG_HOME']) / 'bin/ddlog'),
        'build_driver_sha256': sha256(os.environ['LEMMALOG_DDLOG_BUILD']),
        'native_rustc_sha256': sha256(shutil.which(os.environ.get('RUSTC', 'rustc'))),
        'native_cargo_sha256': sha256(shutil.which(os.environ.get('DDLOG_CARGO', 'cargo'))),
    }


def definition(rules, inputs, outputs, public_output):
    return {'rules': rules, 'schemas': {
        **{name: {'input': True, 'fields': ['int'] * n} for name, n in inputs.items()},
        **{name: {'input': False, 'fields': ['int'] * n} for name, n in outputs.items()}},
        'interface': {'inputs': list(inputs), 'outputs': [public_output]}, 'operation': None}


def eligibility(threshold):
    return definition(f'scratch(P,R,F) :- finding(P,R,F,S), S =< {threshold}.\neligible(P,R,F) :- scratch(P,R,F).',
                      {'finding': 4}, {'scratch': 3, 'eligible': 3}, 'eligible')


def check(name, **data):
    checks.append({'check': name, **data})
    print(name, flush=True)


def read(client, relation, arity):
    return rows(client.call('lemmalog_query', {'predicate': relation})['rows'], arity)


def observe(label, client, relation, arity, wanted, **details):
    response = client.call('lemmalog_query', {'predicate': relation})
    observed = rows(response['rows'], arity)
    snapshots.append({'snapshot': label, 'relation': relation,
        'expected': sorted(wanted), 'observed': sorted(observed),
        'raw_rows': response['rows'], **details})
    assert observed == wanted, label
    return observed


def changes(name, values):
    return [{'op': 'insert', 'predicate': name, 'values': list(v)} for v in sorted(values)]


def reference(program):
    return {'processor_id': program['processor_id'], 'version': program['version']}


def port(node, relation):
    return {'node': node, 'relation': relation}


def save(client, label, authored):
    record = client.call('processor_create', {'definition': authored})
    assert record['validation']['syntax_checked'] and record['validation']['types_checked']
    assert not record['validation']['ddlog_compilation_performed']
    processors[label] = record
    return record


def activate(client, label, record):
    result = client.call('processor_install', reference(record))
    activations.append({'program': label, 'processor': reference(record), 'response': result})
    print(f'activated_native_graph_{len(activations)}: {label}', flush=True)
    return result


def wrapper(inner):
    return {'nodes': {'pipeline': reference(inner)},
        'inputs': {name: {'fields': ['int'] * arity, 'targets': [port('pipeline', name)]}
                   for name, arity in [('findings', 4), ('revisions', 2), ('evidence', 4)]},
        'bindings': [], 'outputs': {'actionable': port('pipeline', 'actionable')}}


def error_text(client, name, arguments):
    response = client.call(name, arguments, error=True)
    return '\n'.join(item['text'] for item in response['content'] if item['type'] == 'text')


def preserve(root, combined_host, outer):
    retained = ARTIFACTS / 'retained-program'
    retained.mkdir(parents=True, exist_ok=True)
    sources = list(combined_host.directory.rglob('program.dl'))
    assert len(sources) == 1
    source = sources[0]
    for name in ('program.dl', 'program_cli', 'build.log'):
        shutil.copy2(source.parent / name, retained / name)
    if (source.parent / 'native-build.log').exists():
        shutil.copy2(source.parent / 'native-build.log', retained / 'native-build.log')
    shutil.copytree(root / 'registry', retained / 'registry', dirs_exist_ok=True)
    (retained / 'processor.json').write_text(json.dumps(outer, indent=2) + '\n')
    (retained / 'resolution.json').write_text(json.dumps(outer['composition'], indent=2) + '\n')
    (retained / 'activation.json').write_text(json.dumps(activations[3], indent=2) + '\n')
    (retained / 'manifest.json').write_text(json.dumps({
        'processor': reference(outer), 'source_sha256': sha256(retained / 'program.dl'),
        'executable_sha256': sha256(retained / 'program_cli'),
        'server_sha256': driver.BINARY_SHA256,
    }, indent=2) + '\n')


def run(root):
    env = dict(os.environ, LEMMALOG_PROCESSOR_REGISTRY=str(root / 'registry'))
    env.pop('LEMMALOG_AGENT_OPERATIONS', None)
    facts = {key: set(map(tuple, FIXTURE[key])) for key in ('findings', 'revisions', 'evidence')}
    oracle = expected(facts)
    host1, host2, host3 = [driver.Host(root, i, env) for i in (1, 2, 3)]
    a, b, c = host1.client(), host2.client(), host3.client()
    p1 = save(a, 'eligibility_v1', eligibility(2))
    activate(a, 'eligibility_v1', p1)
    a.call('apply_changes', {'changes': changes('finding', facts['findings'])})
    observe('individual_eligibility', a, 'eligible', 3, oracle['eligible'])
    p2 = save(b, 'current', definition('scratch(P,R,F) :- eligible(P,R,F), current(P,R).\nselected(P,R,F) :- scratch(P,R,F).',
                                      {'eligible': 3, 'current': 2}, {'scratch': 3, 'selected': 3}, 'selected'))
    activate(b, 'current', p2)
    b.call('apply_changes', {'changes': changes('eligible', oracle['eligible']) + changes('current', facts['revisions'])})
    observe('individual_current', b, 'selected', 3, oracle['selected'])
    p3 = save(c, 'supported', definition('scratch(P,R,F) :- selected(P,R,F), support(P,R,F,W).\nactionable(P,F) :- scratch(P,R,F).',
                                       {'selected': 3, 'support': 4}, {'scratch': 3, 'actionable': 2}, 'actionable'))
    activate(c, 'supported', p3)
    c.call('apply_changes', {'changes': changes('selected', oracle['selected']) + changes('support', facts['evidence'])})
    observe('individual_supported', c, 'actionable', 2, oracle['actionable'])
    check('three_processors_run_independently', actionable=sorted(oracle['actionable']))
    manifest = {'nodes': {'eligibility': reference(p1), 'current': reference(p2), 'supported': reference(p3)},
        'inputs': {
            'findings': {'fields': ['int'] * 4, 'targets': [port('eligibility', 'finding')]},
            'revisions': {'fields': ['int'] * 2, 'targets': [port('current', 'current')]},
            'evidence': {'fields': ['int'] * 4, 'targets': [port('supported', 'support')]}},
        'bindings': [
            {'from': port('eligibility', 'eligible'), 'to': port('current', 'eligible')},
            {'from': port('current', 'selected'), 'to': port('supported', 'selected')}],
        'outputs': {'actionable': port('supported', 'actionable')}}
    h4 = driver.Host(root, 4, env)
    combined = h4.client()
    inner = save(combined, 'inner_v1', {'composition': manifest})
    outer_manifest = wrapper(inner)
    outer = save(combined, 'outer_v1', {'composition': outer_manifest})
    activation = activate(combined, 'outer_v1', outer)
    resolution = outer['composition']
    assert resolution['nodes'] == outer_manifest['nodes']
    assert resolution['dependencies']['pipeline'] == reference(inner)
    assert resolution['dependencies']['pipeline.eligibility'] == reference(p1)
    assert resolution['dependencies']['pipeline.current'] == reference(p2)
    assert resolution['dependencies']['pipeline.supported'] == reference(p3)
    scratch = {physical for physical, origin in resolution['relations'].items()
               if origin.get('kind') == 'processor_relation' and origin.get('relation') == 'scratch'}
    assert len(scratch) == 3
    combined.call('apply_changes', {'changes': sum((changes(k, v) for k, v in facts.items()), [])})
    observe('nested_pipeline_initial', combined, 'actionable', 2, oracle['actionable'])
    assert read(c, 'actionable', 2) == oracle['actionable']
    check('combined_graph_matches_independent_pipeline', distinct_private_scratch_relations=len(scratch),
          nested_program=True, exact_dependencies=resolution['dependencies'])
    combined.call('lemmalog_query', {'predicate': 'scratch'}, error=True)
    combined.call('apply_changes', {'changes': changes('scratch', {(9, 9, 9)})}, error=True)
    check('private_relations_are_not_public_ports')
    for phase in FIXTURE['phases']:
        facts = apply(facts, phase['changes'])
        combined.call('apply_changes', {'changes': phase['changes']})
        observed = observe(phase['name'], combined, 'actionable', 2, expected(facts)['actionable'])
        check(phase['name'], rows=sorted(observed))

    reviewer = h4.client()
    assert reviewer.process.pid != combined.process.pid
    observe('independent_reviewer_before', reviewer, 'actionable', 2, expected(facts)['actionable'])
    review_change = [{'op': 'delete', 'predicate': 'evidence', 'values': [1, 2, 103, 504]}]
    reviewed_facts = apply(facts, review_change)
    assert expected(reviewed_facts)['actionable'] != expected(facts)['actionable']
    reviewer.call('apply_changes', {'changes': review_change})
    observe('independent_reviewer_after_mutation', reviewer, 'actionable', 2, expected(reviewed_facts)['actionable'])
    reviewer.close()
    observe('reviewer_disconnect_retains_mutation', combined, 'actionable', 2, expected(reviewed_facts)['actionable'])
    combined.call('apply_changes', {'changes': [{'op': 'insert', 'predicate': 'evidence', 'values': [1, 2, 103, 504]}]})
    observe('reviewer_fixture_restored', combined, 'actionable', 2, expected(facts)['actionable'])
    check('independent_reviewer_bridge_mutates_shared_program', independent_stdio_processes=True,
          retained_after_reviewer_disconnect=True, mutation=review_change)

    new_leaf = a.call('processor_publish', {'processor_id': p1['processor_id'], 'expected_version': p1['version'], 'definition': eligibility(3)})
    processors['eligibility_v2'] = new_leaf
    resolved = combined.call('processor_get', reference(outer))
    assert resolved['composition']['dependencies']['pipeline.eligibility'] == reference(p1)
    observe('old_nested_pin_after_leaf_publish', combined, 'actionable', 2, expected(facts, 2)['actionable'])
    new_manifest = json.loads(json.dumps(manifest))
    new_manifest['nodes']['eligibility'] = reference(new_leaf)
    new_inner = combined.call('processor_publish', {'processor_id': inner['processor_id'],
        'expected_version': inner['version'], 'definition': {'composition': new_manifest}})
    processors['inner_v2'] = new_inner
    combined.call('processor_publish', {'processor_id': inner['processor_id'],
        'expected_version': inner['version'], 'definition': {'composition': manifest}}, error=True)
    new_outer_manifest = wrapper(new_inner)
    new_outer = combined.call('processor_publish', {'processor_id': outer['processor_id'],
        'expected_version': outer['version'], 'definition': {'composition': new_outer_manifest}})
    processors['outer_v2'] = new_outer
    assert combined.call('instance_info')['processor']['version'] == outer['version']
    h5 = driver.Host(root, 5, env)
    fresh = h5.client()
    new_activation = activate(fresh, 'outer_v2', new_outer)
    fresh.call('apply_changes', {'changes': sum((changes(k, v) for k, v in facts.items()), [])})
    new_rows = observe('new_nested_version', fresh, 'actionable', 2, expected(facts, 3)['actionable'])
    old_rows = observe('old_nested_version_still_pinned', combined, 'actionable', 2, expected(facts, 2)['actionable'])
    assert old_rows != new_rows
    check('dependency_and_composition_versions_remain_pinned', old_rows=sorted(old_rows), new_rows=sorted(new_rows))

    archive_args = {**reference(new_leaf), 'expected_version': new_leaf['version'], 'expected_revision': 0}
    archive_args.pop('version')
    archived = combined.call('processor_archive', archive_args)
    assert archived['status'] == 'archived' and archived['lifecycle_revision'] == 1
    assert all(row['processor_id'] != p1['processor_id'] for row in combined.call('processor_list', {'limit': 100})['processors'])
    assert all(row['processor_id'] != p1['processor_id'] for row in combined.call('processor_search', {'query': p1['processor_id'], 'limit': 100})['processors'])
    archived_rows = [row for row in combined.call('processor_search', {'query': p1['processor_id'], 'limit': 100, 'include_archived': True})['processors']
                     if row['processor_id'] == p1['processor_id']]
    assert len(archived_rows) == 1 and archived_rows[0]['status'] == 'archived'
    assert archived_rows[0]['lifecycle_revision'] == 1
    assert combined.call('processor_get', reference(inner)) == inner
    assert combined.call('processor_get', reference(outer)) == outer
    assert combined.call('processor_get', reference(new_outer)) == new_outer
    observe('archive_keeps_old_live_graph', combined, 'actionable', 2, old_rows)
    observe('archive_keeps_new_live_graph', fresh, 'actionable', 2, new_rows)
    restored = combined.call('processor_restore', {'processor_id': p1['processor_id'],
        'expected_version': new_leaf['version'], 'expected_revision': 1})
    assert restored['status'] == 'active' and restored['lifecycle_revision'] == 2
    no_op = combined.call('processor_restore', {'processor_id': p1['processor_id'],
        'expected_version': new_leaf['version'], 'expected_revision': 2})
    assert no_op == restored
    stale = error_text(combined, 'processor_archive', archive_args)
    for token in ('expected 0', 'current 2', 'latest', 'reconsider'):
        assert token in stale, stale
    restored_use = save(combined, 'restored_new_use', {'composition': new_outer_manifest})
    assert restored_use['version'] == new_outer['version']
    restored_rows = [row for row in combined.call('processor_search', {'query': p1['processor_id'], 'limit': 100})['processors']
                     if row['processor_id'] == p1['processor_id']]
    assert len(restored_rows) == 1 and restored_rows[0]['status'] == 'active'
    check('archive_restore_preserves_live_graphs_and_historical_refs', archive=archived, restore=restored,
          restored_noop=no_op, stale_request_error=stale, restored_definition_reference=reference(restored_use),
          no_additional_native_activation=True)
    assert len(activations) == 5
    preserve(root, h4, outer)
    for host in driver.hosts:
        host.stop()
        assert not host.descriptor.exists()
        for source in host.directory.rglob('program.dl'):
            executable = source.parent / 'program_cli'
            assert executable.is_file()
            build = {'instance': host.directory.name, 'source': source.read_text(),
                'source_sha256': sha256(source), 'executable_sha256': sha256(executable),
                'build_log_sha256': sha256(source.parent / 'build.log')}
            if host is h4:
                assert build['source_sha256'] == activation['composition']['generated_source_sha256'] == outer['composition']['generated_source_sha256']
                assert all(f'relation R_{relation}(' in build['source'] for relation in scratch)
                build['private_scratch_relations'] = sorted(scratch)
            if host is h5:
                assert build['source_sha256'] == new_activation['composition']['generated_source_sha256'] == new_outer['composition']['generated_source_sha256']
            builds.append(build)
    assert len(builds) == 5
    check('five_actual_programs_stopped_cleanly', actual_graph_activations=len(activations))


if __name__ == '__main__':
    passed = False
    start_identity, end_identity = {}, {}
    ARTIFACTS.mkdir(mode=0o700, parents=True, exist_ok=True)
    REPORT.parent.mkdir(parents=True, exist_ok=True)
    try:
        start_identity = toolchain_identity()
        assert start_identity['server_sha256'] == EXPECTED_SERVER_SHA256, 'Frozen Rust server hash changed'
        with tempfile.TemporaryDirectory(prefix='lmcompose-', dir='/tmp') as directory:
            try:
                run(Path(directory))
            except Exception:
                failed = ARTIFACTS / 'failed-run'
                shutil.copytree(directory, failed, dirs_exist_ok=True, symlinks=True,
                                ignore=lambda directory, names: [name for name in names if Path(directory, name).is_socket()])
                raise
            finally:
                errors = []
                for host in driver.hosts:
                    if not host.log.closed:
                        try: host.stop()
                        except Exception as exc: errors.append(str(exc))
                for client in driver.clients:
                    if not client.errors.closed:
                        try: client.close()
                        except Exception as exc: errors.append(str(exc))
                if errors: raise AssertionError('; '.join(errors))
            end_identity = toolchain_identity()
            assert start_identity == end_identity, 'Compiler or backend bytes changed during acceptance'
            passed = True
    finally:
        REPORT.write_text(json.dumps({'passed': passed, 'checks': checks,
            'processors': processors, 'activations': activations, 'snapshots': snapshots,
            'generated_programs': builds, 'toolchain_start': start_identity, 'toolchain_end': end_identity,
            'server_sha256': driver.BINARY_SHA256,
            'native_build_evidence': json.loads(Path(os.environ['COMPOSITION_NATIVE_EVIDENCE']).read_text()) if os.environ.get('COMPOSITION_NATIVE_EVIDENCE') else None,
            'activation_mode': 'verified reuse of captured native executables' if os.environ.get('COMPOSITION_NATIVE_EVIDENCE') else 'native compilation',
            'scope': 'deterministic native DDlog acceptance; independent finite-set oracle; nested ordinary programs; fresh reviewer pipe bridge; no provider or model-authorship claim'}, indent=2) + '\n')
    print('PASS: composition acceptance', flush=True)
