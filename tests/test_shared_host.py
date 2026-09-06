"""Simulated-runtime host contracts, deliberately separate from real DDlog evidence.

Run after cargo build --features mcp --bin lemmalog-ddlog-mcp:
  python3 -m unittest discover -s tests -p test_shared_host.py -v
"""
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('shared_driver', ROOT / 'scripts/test-shared-instance.py')
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)
DEFINITION = {'rules': 'echo(V) :- source(V).', 'schemas': {
    'source': {'input': True, 'fields': ['string']},
    'echo': {'input': False, 'fields': ['string']}}}

class SharedHostFailures(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='lmsfake-', dir='/tmp')
        self.root = Path(self.tmp.name)
        self.control = self.root / 'control'
        self.control.mkdir()
        self.env = dict(os.environ, FAKE_CONTROL=str(self.control),
                        LEMMALOG_DDLOG_BUILD=str(ROOT / 'tests/fixtures/shared_fake_build.py'))
        self.env.pop('LEMMALOG_PROCESSOR_REGISTRY', None)
        self.env.pop('LEMMALOG_AGENT_OPERATIONS', None)
        self.host = driver.Host(self.root, 1, self.env)
        self.clients = []
        self.raw = []

    def tearDown(self):
        try:
            self.host.stop()
        finally:
            for client in self.clients:
                if client.process.poll() is None:
                    client.close()
            for stream in self.raw:
                stream.close()
            self.tmp.cleanup()

    def client(self):
        client = self.host.client()
        self.clients.append(client)
        return client

    def install(self):
        client = self.client()
        client.call('lemmalog_install_rules', DEFINITION)
        return client

    def raw_client(self):
        stream = socket.socket(socket.AF_UNIX)
        stream.settimeout(5)
        stream.connect(str(self.host.socket))
        stream.sendall((json.dumps({'kind': 'attach', 'instance_id': self.host.identity}) + '\n').encode())
        response = b''
        while not response.endswith(b'\n'):
            response += stream.recv(1)
        self.assertTrue(json.loads(response)['attached'])
        self.raw.append(stream)
        return stream

    def send(self, stream, name, arguments):
        stream.sendall((json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call',
                                   'params': {'name': name, 'arguments': arguments}}) + '\n').encode())

    def wait_for(self, condition, seconds=5):
        deadline = time.monotonic() + seconds
        while not condition():
            self.assertLess(time.monotonic(), deadline, 'Condition deadline exceeded')
            time.sleep(0.02)

    def events(self, name):
        path = self.control / 'events.jsonl'
        return [e for e in map(json.loads, path.read_text().splitlines()) if e['event'] == name]

    def test_partial_oversize_and_invalid_json_preserve_other_client(self):
        client = self.install()
        partial = self.raw_client()
        partial.sendall(b'{"jsonrpc":"2.0","id":1,"method":"tools/call"')
        partial.shutdown(socket.SHUT_WR)
        self.assertEqual(partial.recv(1), b'')
        oversized = self.raw_client()
        try:
            oversized.sendall(b'x' * (1024 * 1024 + 1))
        except (BrokenPipeError, ConnectionResetError):
            pass
        oversized.close()
        bad = self.raw_client()
        bad.sendall(b'{bad}\n')
        response = json.loads(bad.makefile('rb').readline())
        self.assertEqual(response['error']['code'], -32700)
        driver.mutation(client, 'valid')
        self.assertEqual(driver.query(client), {driver.row('valid')})
        self.assertEqual(len(self.events('mutation')), 1)

    def test_lost_response_commits_once_without_automatic_retry(self):
        client = self.install()
        (self.control / 'hold_mutation').touch()
        lost = self.raw_client()
        self.send(lost, 'apply_changes', {'changes': [{'op': 'insert', 'predicate': 'source', 'values': ['once']}]})
        self.wait_for(lambda: (self.control / 'mutation_entered').exists())
        lost.close()
        (self.control / 'hold_mutation').unlink()
        self.assertEqual(driver.query(client), {driver.row('once')})
        self.assertEqual(len(self.events('mutation')), 1)
        reconnected = self.client()
        self.assertEqual(driver.query(reconnected), {driver.row('once')})

    def test_uncertain_runtime_failure_survives_reconnect_as_failed(self):
        client = self.install()
        (self.control / 'fail_mutation').touch()
        client.call('apply_changes', {'changes': [{'op': 'insert', 'predicate': 'source', 'values': ['uncertain']}]}, error=True)
        new = self.client()
        self.assertEqual(new.call('instance_info')['health'], 'failed')
        new.call('lemmalog_install_rules', DEFINITION, error=True)
        new.call('apply_changes', {'changes': []}, error=True)
        self.assertEqual(len(self.events('build')), 1)
        self.assertEqual(len(self.events('mutation')), 1)

    def test_failed_candidate_preserves_live_program(self):
        client = self.install()
        driver.mutation(client, 'retained')
        (self.control / 'reject_build').touch()
        client.call('lemmalog_install_rules', DEFINITION, error=True)
        self.assertEqual(driver.query(client), {driver.row('retained')})
        self.assertEqual(client.call('instance_info')['health'], 'ready')

    def test_stale_descriptor_rejected_and_permissions_enforced(self):
        self.assertEqual(self.host.descriptor.stat().st_mode & 0o777, 0o600)
        copied = self.root / 'old.json'
        desc = json.loads(self.host.descriptor.read_text())
        desc['instance_id'] = 'old-incarnation'
        copied.write_text(json.dumps(desc)); copied.chmod(0o600)
        result = subprocess.run([driver.BINARY, 'connect', '--descriptor', str(copied)], input=b'', capture_output=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b'mismatch', result.stderr)
        copied.chmod(0o644)
        result = subprocess.run([driver.BINARY, 'connect', '--descriptor', str(copied)], input=b'', capture_output=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b'private', result.stderr)
        self.assertEqual(self.client().call('instance_info')['instance_id'], self.host.identity)

    def test_stop_admitted_with_sixty_four_idle_clients(self):
        for _ in range(64):
            self.raw_client()
        self.host.stop()
        self.assertFalse(self.host.descriptor.exists())

    def test_standalone_children_remain_in_client_process_group(self):
        (self.control / 'hold_build').touch()
        env = dict(self.env, LEMMALOG_DDLOG_WORKDIR=str(self.root / 'standalone-builds'))
        process = subprocess.Popen([driver.BINARY], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, env=env, start_new_session=True)
        try:
            request = {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call',
                       'params': {'name': 'lemmalog_install_rules', 'arguments': DEFINITION}}
            process.stdin.write((json.dumps(request) + '\n').encode()); process.stdin.flush()
            self.wait_for(lambda: (self.control / 'build_child').exists())
            self.assertEqual(self.events('build')[0]['pgid'], process.pid)
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=5)
            self.wait_for(lambda: not group_exists(process.pid))
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
            process.stdin.close(); process.stdout.close(); process.stderr.close()

    def test_stop_interrupts_stalled_compiler_and_descendant(self):
        (self.control / 'hold_build').touch()
        stream = self.raw_client()
        self.send(stream, 'lemmalog_install_rules', DEFINITION)
        self.wait_for(lambda: (self.control / 'build_child').exists())
        group = self.events('build')[0]['pgid']
        started = time.monotonic()
        self.host.stop()
        self.assertLess(time.monotonic() - started, 5)
        self.wait_for(lambda: not group_exists(group))

    def test_stop_interrupts_stalled_runtime(self):
        self.install()
        (self.control / 'hold_mutation').touch()
        stream = self.raw_client()
        self.send(stream, 'apply_changes', {'changes': []})
        self.wait_for(lambda: (self.control / 'mutation_entered').exists())
        group = self.events('runtime')[0]['pgid']
        self.host.stop()
        self.wait_for(lambda: not group_exists(group))

    def test_sigterm_immediately_after_ready_cleans_endpoint(self):
        # No MCP initialization or compiler work separates readiness from termination.
        self.host.process.send_signal(signal.SIGTERM)
        self.host.process.wait(timeout=5)
        self.assertEqual(self.host.process.returncode, 0)
        self.assertFalse(self.host.socket.exists())
        self.assertFalse(self.host.descriptor.exists())

    def test_sigterm_cleans_runtime_and_endpoint(self):
        self.install()
        group = self.events('runtime')[0]['pgid']
        self.host.process.send_signal(signal.SIGTERM)
        self.host.process.wait(timeout=5)
        self.assertFalse(self.host.socket.exists())
        self.wait_for(lambda: not group_exists(group))

    def test_slow_reader_does_not_hold_semantic_owner(self):
        client = self.install()
        (self.control / 'slow_output').touch()
        stream = self.raw_client()
        self.send(stream, 'lemmalog_query', {'predicate': 'echo'})
        self.wait_for(lambda: bool(self.events('query')))
        started = time.monotonic()
        self.assertEqual(client.call('instance_info')['health'], 'ready')
        self.assertLess(time.monotonic() - started, 2)

    def test_runtime_output_bound_invalidates_oversized_response(self):
        client = self.install()
        (self.control / 'huge_output').touch()
        client.call('lemmalog_query', {'predicate': 'echo'}, error=True)
        self.assertEqual(client.call('instance_info')['health'], 'failed')


def group_exists(group):
    try:
        os.killpg(group, 0)
        return True
    except ProcessLookupError:
        return False

if __name__ == '__main__':
    unittest.main()
