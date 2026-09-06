#!/usr/bin/env python3
"""Independent pipe clients against an actual shared DDlog host.

Requires the same operator-supplied DDlog build environment as test-ddlog-mcp.py.
No PTY, provider calls, or simulated graph evaluation is used.
"""
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time

BINARY = os.path.abspath(os.environ.get('LEMMALOG_DDLOG_MCP', 'target/debug/lemmalog-ddlog-mcp'))
BINARY_SHA256 = hashlib.sha256(Path(BINARY).read_bytes()).hexdigest()
TIMEOUT = float(os.environ.get('SHARED_TEST_TIMEOUT', '180'))
RECEIPT = Path(os.environ.get('SHARED_INSTANCE_RECEIPT', '/tmp/shared-instance-receipt.json'))
checks = []
builds = []
hosts = []
clients = []


def checked(name, **evidence):
    checks.append({'check': name, **evidence})


class Client:
    def __init__(self, descriptor, env):
        self.errors = tempfile.TemporaryFile()
        self.process = subprocess.Popen(
            [BINARY, 'connect', '--descriptor', str(descriptor)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.errors,
            env=env, bufsize=0)
        self.lines = queue.Queue()
        self.sequence = 0
        def read():
            try:
                while True:
                    line = self.process.stdout.readline()
                    self.lines.put(line)
                    if not line:
                        break
            except Exception as exc:
                self.lines.put(exc)
        self.reader = threading.Thread(target=read, daemon=True)
        self.reader.start()
        clients.append(self)

    def rpc(self, method, params, fragmented=False):
        self.sequence += 1
        request = {'jsonrpc': '2.0', 'id': self.sequence, 'method': method, 'params': params}
        payload = (json.dumps(request, separators=(',', ':')) + '\n').encode()
        # Fragment large requests deliberately while preserving one JSON-RPC line.
        size = 997 if fragmented else len(payload)
        for start in range(0, len(payload), size):
            remaining = memoryview(payload)[start:start + size]
            while remaining:
                count = self.process.stdin.write(remaining)
                if not count:
                    raise RuntimeError('Bridge stdin stopped accepting bytes')
                remaining = remaining[count:]
        line = self.lines.get(timeout=TIMEOUT)
        if not isinstance(line, bytes) or not line:
            self.errors.seek(0)
            detail = self.errors.read().decode(errors='replace')
            raise AssertionError(f'Bridge exited or failed: {line!r}: {detail}')
        response = json.loads(line)
        assert response['id'] == request['id'], response
        assert 'error' not in response, response
        return response['result'], len(payload), hashlib.sha256(payload).hexdigest()

    def call(self, name, args=None, error=False, fragmented=False):
        result, size, digest = self.rpc('tools/call', {'name': name, 'arguments': args or {}}, fragmented)
        assert result.get('isError', False) == error, result
        if error:
            return result
        return json.loads(result['content'][0]['text'])

    def initialize(self):
        result, _, _ = self.rpc('initialize', {})
        assert result['serverInfo']['name']

    def close(self):
        if self.process.stdin and not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
            raise AssertionError('Bridge did not exit after input EOF')
        self.process.stdout.close()
        self.errors.close()


class Host:
    def __init__(self, root, index, env):
        self.directory = root / f'h{index}'
        self.directory.mkdir(mode=0o700)
        self.socket = self.directory / 'socket'
        self.descriptor = self.directory / 'descriptor.json'
        self.env = dict(env, LEMMALOG_DDLOG_WORKDIR=str(self.directory / 'build'))
        self.log = open(self.directory / 'host.log', 'wb')
        self.process = subprocess.Popen(
            [BINARY, 'host', '--socket', str(self.socket), '--descriptor', str(self.descriptor)],
            stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log,
            env=self.env, start_new_session=True)
        hosts.append(self)
        deadline = time.monotonic() + 10
        while not self.descriptor.exists():
            assert self.process.poll() is None, 'Host exited before readiness'
            assert time.monotonic() < deadline, 'Host readiness deadline exceeded'
            time.sleep(0.025)
        self.identity = json.loads(self.descriptor.read_text())['instance_id']
        assert self.socket.stat().st_mode & 0o777 == 0o600
        assert self.directory.stat().st_mode & 0o777 == 0o700

    def client(self):
        client = Client(self.descriptor, self.env)
        client.initialize()
        info = client.call('instance_info')
        assert info['instance_id'] == self.identity
        return client

    def stop(self):
        if self.process.poll() is None:
            stop = subprocess.run([BINARY, 'stop', '--descriptor', str(self.descriptor)],
                                  env=self.env, capture_output=True, timeout=20)
            assert stop.returncode == 0, stop.stderr.decode()
            self.process.wait(timeout=10)
        assert self.process.returncode == 0, 'Host shutdown was not clean'
        assert not self.socket.exists(), 'Host socket survived stop'
        self.log.close()


def row(value):
    return 'R_echo{.f0 = ' + json.dumps(value, ensure_ascii=False) + '}'


def query(client):
    return set(client.call('lemmalog_query', {'predicate': 'echo'})['rows'].splitlines())


def mutation(client, value, op='insert'):
    return client.call('apply_changes', {'changes': [{'op': op, 'predicate': 'source', 'values': [value]}]})


def run(root):
    env = dict(os.environ, LEMMALOG_PROCESSOR_REGISTRY=str(root / 'registry'))
    operations = root / 'operations.json'
    operations.write_text(json.dumps({'review': {'version': 'v1', 'description': 'Deterministic test operation'}}))
    env['LEMMALOG_AGENT_OPERATIONS'] = str(operations)
    definition = {'rules': 'echo(V) :- source(V).', 'schemas': {
        'source': {'input': True, 'fields': ['string']},
        'echo': {'input': False, 'fields': ['string']}}, 'operation': None}
    h1 = Host(root, 1, env)
    a, b = h1.client(), h1.client()
    assert a.process.pid != b.process.pid
    checked('independent_clients_same_instance', reused_initial_rpc_id=1)
    original = a.call('processor_create', {'definition': definition})
    assert original['validation'] == {'syntax_checked': True, 'types_checked': True, 'supported_lowering_checked': True, 'ddlog_compilation_performed': False}
    pid, version = original['processor_id'], original['version']
    a.call('processor_install', {'processor_id': pid, 'version': version})
    mutation(a, 'author')
    assert query(b) == {row('author')}
    assert 'author' in b.call('lemmalog_why', {'rule': 0})['bindings']
    checked('reviewer_observes_authored_graph_and_witness')
    large = 'x' * (256 * 1024)
    args = {'changes': [{'op': 'insert', 'predicate': 'source', 'values': [large]}]}
    result, size, digest = b.rpc('tools/call', {'name': 'apply_changes', 'arguments': args}, fragmented=True)
    assert not result.get('isError', False), result
    assert query(a) == {row('author'), row(large)}
    checked('large_fragmented_transaction_intact', request_bytes=size, request_sha256=digest)
    mutation(a, large, 'delete')
    a.close()
    mutation(b, 'reviewer')
    assert query(b) == {row('author'), row('reviewer')}
    a = h1.client()
    assert query(a) == query(b)
    checked('disconnect_reconnect_retains_live_state')
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        futures = [pool.submit(mutation, a, 'parallel-a'), pool.submit(mutation, b, 'parallel-b')]
        for future in futures:
            future.result(timeout=TIMEOUT)
    assert query(a) == {row(v) for v in ['author', 'reviewer', 'parallel-a', 'parallel-b']}
    checked('concurrent_independent_writes_serialize')
    changed = dict(definition, rules='echo(V) :- source(V), source("gate").')
    updated = b.call('processor_publish', {'processor_id': pid, 'expected_version': version, 'definition': changed})
    assert updated['version'] != version
    a.call('processor_publish', {'processor_id': pid, 'expected_version': version, 'definition': definition}, error=True)
    assert a.call('instance_info')['processor']['version'] == version
    assert row('author') in query(a)
    checked('conditional_registry_update_and_running_version_pin')
    fork = a.call('processor_fork', {'processor_id': pid, 'version': version})
    assert fork['processor_id'] != pid
    assert fork['lineage'] == {'processor_id': pid, 'version': version}
    h2 = Host(root, 2, env)
    c = h2.client()
    assert h2.identity != h1.identity
    c.call('processor_install', {'processor_id': fork['processor_id'], 'version': fork['version']})
    assert query(c) == set()
    mutation(c, 'isolated')
    assert query(c) == {row('isolated')}
    assert row('isolated') not in query(a)
    checked('fork_lineage_and_instance_isolation')
    h3 = Host(root, 3, env)
    d, e = h3.client(), h3.client()
    registered = d.call('processor_create', {'definition': {
        'operation': {'name': 'review', 'version': 'v1', 'description': 'Deterministic test operation'},
        'rules': 'reviewed(E,R,O) :- agent_result(E,R,O).',
        'schemas': {'reviewed': {'input': False, 'fields': ['string', 'int', 'string']}}}})
    d.call('processor_install', {'processor_id': registered['processor_id'], 'version': registered['version']})
    assert e.call('instance_info')['processor']['version'] == registered['version']
    request = d.call('submit_agent_input', {'entity': 'item', 'revision': 1, 'payload': 'first'})['request_id']
    e.call('claim_agent_request', {'request_id': request})
    e.close()
    d.call('claim_agent_request', {'request_id': request}, error=True)
    current = d.call('submit_agent_input', {'entity': 'item', 'revision': 2, 'payload': 'second'})['request_id']
    e = h3.client()
    stale = e.call('complete_agent_request', {'request_id': request, 'output': 'old'})
    assert stale['fresh'] is False
    assert d.call('lemmalog_query', {'predicate': 'reviewed'})['rows'] == ''
    e.call('claim_agent_request', {'request_id': current})
    assert e.call('complete_agent_request', {'request_id': current, 'output': 'fresh'})['fresh']
    assert d.call('lemmalog_query', {'predicate': 'reviewed'})['rows'].strip() == 'R_reviewed{.f0 = "item", .f1 = 2, .f2 = "fresh"}'
    checked('shared_claim_survives_disconnect_and_stale_response_excluded')
    for host in hosts:
        host.stop()
        for source in host.directory.rglob('program.dl'):
            executable = source.parent / 'program_cli'
            assert executable.is_file(), 'Successful graph executable missing'
            builds.append({'instance': host.directory.name, 'source': source.read_text(),
                'source_sha256': hashlib.sha256(source.read_bytes()).hexdigest(),
                'executable_sha256': hashlib.sha256(executable.read_bytes()).hexdigest()})
    assert len(builds) == 3, 'Expected three real compiled graphs'
    checked('explicit_shutdown_removes_all_endpoints')


if __name__ == '__main__':
    passed = False
    try:
        with tempfile.TemporaryDirectory(prefix='lmshare-', dir='/tmp') as directory:
            try:
                run(Path(directory))
            except Exception:
                for host in hosts:
                    log_path = host.directory / 'host.log'
                    if log_path.exists():
                        print(log_path.read_text(errors='replace')[-12000:], file=sys.stderr)
                raise
            finally:
                cleanup_errors = []
                for host in hosts:
                    if not host.log.closed:
                        try:
                            host.stop()
                        except Exception as exc:
                            cleanup_errors.append(f'Host cleanup: {exc}')
                for client in clients:
                    if not client.errors.closed:
                        try:
                            client.close()
                        except Exception as exc:
                            cleanup_errors.append(f'Client cleanup: {exc}')
                if cleanup_errors:
                    raise AssertionError('; '.join(cleanup_errors))
            assert hashlib.sha256(Path(BINARY).read_bytes()).hexdigest() == BINARY_SHA256, 'Backend executable changed during run'
            passed = True
    finally:
        RECEIPT.write_text(json.dumps({'passed': passed, 'checks': checks, 'generated_programs': builds,
            'backend_executable_sha256': BINARY_SHA256,
            'ddlog_compiler_sha256': hashlib.sha256((Path(os.environ['DDLOG_HOME']) / 'bin/ddlog').read_bytes()).hexdigest(),
            'backend': 'official DDlog-generated Differential/Timely',
            'scope': 'deterministic independent clients; no provider inference, durability, or LLM authorship claim'}, indent=2) + '\n')
    print(f'PASS: {len(checks)} shared-instance checks')
