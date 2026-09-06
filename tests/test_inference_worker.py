"""External worker contracts; no provider, native compiler, or live host calls."""
import asyncio
import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('inference_worker', ROOT / 'scripts/lemmalog_inference_worker.py')
worker = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = worker
spec.loader.exec_module(worker)


def config_dict():
    return {'format_version': 1, 'operation': 'review', 'endpoint': 'https://model.example.invalid/v1/chat/completions',
            'model': 'test-model', 'system_prompt': 'Return a short answer.'}


def config():
    return worker.InferenceConfig.from_dict(config_dict())


def request_id(configuration, entity='a', revision=1, payload='hello'):
    return json.dumps([configuration.operation, configuration.operation_binding()['version'], entity, revision, payload],
                      separators=(',', ':'))


class FakeMCP:
    def __init__(self, configuration):
        self.configuration = configuration
        self.binding = configuration.operation_binding()
        self.host_binding = dict(self.binding)
        self.pin = {'processor_id': 'processor_example', 'version': 'sha256:example'}
        self.instance_id = 'instance-one'
        self.claimed, self.completed, self.calls = set(), [], []
        self.fresh, self.bad_claim = True, False
        self.fail_settle, self.bridge_exit_code = False, None

    async def call(self, name, arguments=None):
        arguments = arguments or {}
        self.calls.append((name, arguments.copy()))
        if name == 'instance_info':
            return {'instance_id': self.instance_id, 'health': 'ready', 'processor': self.pin.copy()}
        if name == 'processor_get':
            assert arguments == self.pin
            return {**self.pin, 'definition': {'operation': self.binding.copy()}}
        if name == 'agent_operations':
            return {'operations': [{**self.host_binding, 'input': 'string', 'output': 'string'}]}
        if name == 'claim_agent_request':
            identity = arguments['request_id']
            if identity in self.claimed:
                raise worker.WorkerError('Already claimed or completed; no automatic replay')
            self.claimed.add(identity)
            operation, version, entity, revision, payload = json.loads(identity)
            return {'request_id': identity, 'operation': operation,
                    'operation_version': 'wrong-version' if self.bad_claim else version,
                    'entity': entity, 'revision': revision, 'payload': payload, 'status': 'claimed'}
        if name == 'complete_agent_request':
            if self.fail_settle:
                raise worker.WorkerError('Settlement response lost; reconcile host state', uncertain=True)
            self.completed.append(arguments.copy())
            return {'request_id': arguments['request_id'], 'duplicate': False, 'fresh': self.fresh,
                    'transaction': {'version': 1, 'deltas': ''}}
        raise AssertionError(name)

    async def close(self):
        self.bridge_exit_code = 0


class FakeProvider:
    def __init__(self, configuration, fail=False, content='answer\nsecond line'):
        self.configuration, self.fail, self.content = configuration, fail, content
        self.calls, self.active, self.max_active = [], 0, 0

    async def infer(self, payload):
        self.calls.append(payload)
        self.active += 1
        self.max_active = max(self.max_active, self.active)
        try:
            await asyncio.sleep(0.01)
            if self.fail:
                raise worker.WorkerError('Provider outcome uncertain; reconcile before resubmission', uncertain=True)
            return worker.ProviderResult(self.content, self.configuration.model, 'response-one',
                {'prompt_tokens': 3, 'completion_tokens': 4, 'total_tokens': 7}, 'stop',
                'a' * 64, 'b' * 64, self.configuration.config_sha256)
        finally:
            self.active -= 1


class ConfigTests(unittest.TestCase):
    def test_canonical_public_hash_binding_and_defaults(self):
        original = config()
        public = original.public_dict()
        self.assertEqual(public['max_tokens'], 32768)
        self.assertEqual((public['temperature'], public['top_p']), (0.3, 0.9))
        self.assertEqual((public['reasoning_effort'], public['reasoning_enabled']), ('high', True))
        digest = hashlib.sha256(json.dumps(public, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()).hexdigest()
        self.assertEqual(original.config_sha256, digest)
        binding = original.operation_binding()
        self.assertEqual(binding['version'], 'sha256:' + digest)
        self.assertEqual(original.operation_registry(), {binding['name']: {key: binding[key] for key in ('version', 'description')}})
        self.assertEqual(worker.InferenceConfig.from_dict(dict(reversed(list(public.items())))).operation_binding(), binding)
        changed = dict(public, system_prompt='Changed public code configuration')
        self.assertNotEqual(worker.InferenceConfig.from_dict(changed).operation_binding(), binding)

    def test_strict_schema_endpoint_and_numeric_bounds(self):
        for field, value in [('api_key', 'DO_NOT_ECHO_SECRET'), ('headers', {'Authorization': 'DO_NOT_ECHO_SECRET'}),
                             ('format_version', True), ('max_tokens', 32769), ('max_tokens', True),
                             ('temperature', float('nan')), ('top_p', 0), ('reasoning_enabled', 1),
                             ('reasoning_effort', 'unbounded'), ('model', '')]:
            with self.subTest(field=field), self.assertRaises(ValueError) as raised:
                worker.InferenceConfig.from_dict(dict(config_dict(), **{field: value}))
            self.assertNotIn('DO_NOT_ECHO_SECRET', str(raised.exception))
        for endpoint in ('http://host/path', 'https://u:DO_NOT_ECHO_SECRET@host/path',
                         'https://host/path?token=DO_NOT_ECHO_SECRET', 'https://host/path#DO_NOT_ECHO_SECRET',
                         'https://host/\npath', 'https://host/\x00path', 'https:///path'):
            with self.assertRaises(ValueError) as raised:
                worker.InferenceConfig.from_dict(dict(config_dict(), endpoint=endpoint))
            self.assertNotIn('DO_NOT_ECHO_SECRET', str(raised.exception))

    def test_cli_does_not_offer_or_echo_credentials(self):
        parser = worker.build_parser()
        args = parser.parse_args(['--config', 'config.json', '--descriptor', 'descriptor.json',
                                  '--binary', 'server', '--request-id', 'opaque-id', '--receipt', 'receipt.json'])
        self.assertEqual(args.max_concurrency, 2)
        self.assertEqual(args.modal_bin, 'modal')
        output = io.StringIO()
        with contextlib.redirect_stderr(output), self.assertRaises(SystemExit):
            parser.parse_args(['--api-key', 'DO_NOT_ECHO_SECRET'])
        self.assertNotIn('DO_NOT_ECHO_SECRET', output.getvalue())
        self.assertNotIn('--api-key', parser.format_help())

    def test_duplicate_config_fields_are_rejected_without_echoing_values(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config.json'
            path.write_text('{"operation":"review","operation":"DO_NOT_ECHO_SECRET"}')
            with self.assertRaises(ValueError) as raised:
                worker.InferenceConfig.load(path)
            self.assertNotIn('DO_NOT_ECHO_SECRET', str(raised.exception))


class WorkerTests(unittest.IsolatedAsyncioTestCase):
    async def test_cli_retains_paid_response_when_combined_completion_is_oversized(self):
        cfg = config()
        client = FakeMCP(cfg)
        provider = FakeProvider(cfg, content='r' * 100_000)
        payload = 'p' * 950_000
        identity = request_id(cfg, payload=payload)
        self.assertLess(len(worker._frame(1, 'tools/call', {'name': 'claim_agent_request',
                            'arguments': {'request_id': identity}})), worker.MCP_REQUEST_LIMIT)
        self.assertLess(len(provider.content.encode()), worker.MCP_REQUEST_LIMIT)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config.json'
            path.write_text(json.dumps(cfg.public_dict()))
            args = worker.build_parser().parse_args(['--config', str(path), '--descriptor', 'descriptor',
                '--binary', 'server', '--request-id', identity, '--receipt', str(Path(directory) / 'receipt.json')])
            with patch.object(worker.MCPPipe, 'connect', return_value=client), patch.object(worker, 'ProviderClient', return_value=provider):
                receipt = await worker._cli(args)
        self.assertFalse(receipt['passed'])
        result = receipt['results'][0]
        self.assertFalse(result['completed'])
        self.assertIn('1 MiB', result['error'])
        self.assertIn('prepared', result, 'Already-paid provider response must survive local completion rejection')
        self.assertEqual(result['prepared']['request_id'], identity)
        self.assertEqual(result['prepared']['provider']['content'], provider.content)
        self.assertEqual(result['prepared']['provider']['response_sha256'], 'b' * 64)
        self.assertEqual(provider.calls, [payload])
        self.assertEqual(client.completed, [])

    async def test_cli_rejects_empty_successful_settlement_and_retains_prepared(self):
        cfg = config()
        client, provider = FakeMCP(cfg), FakeProvider(cfg)
        original_call = client.call
        async def malformed(name, arguments=None):
            if name == 'complete_agent_request':
                return {}
            return await original_call(name, arguments)
        client.call = malformed
        identity = request_id(cfg)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config.json'
            path.write_text(json.dumps(cfg.public_dict()))
            args = worker.build_parser().parse_args(['--config', str(path), '--descriptor', 'descriptor',
                '--binary', 'server', '--request-id', identity, '--receipt', str(Path(directory) / 'receipt.json')])
            with patch.object(worker.MCPPipe, 'connect', return_value=client), patch.object(worker, 'ProviderClient', return_value=provider):
                receipt = await worker._cli(args)
        self.assertFalse(receipt['passed'], 'An empty success payload does not establish settlement')
        result = receipt['results'][0]
        self.assertFalse(result['completed'])
        self.assertTrue(result['uncertain'])
        self.assertEqual(result['prepared']['provider']['content'], provider.content)
        self.assertEqual(provider.calls, ['hello'])

    async def test_bind_verifies_exact_pin_and_three_field_operation_before_provider(self):
        cfg = config()
        for mismatch in ('definition', 'host'):
            client, provider = FakeMCP(cfg), FakeProvider(cfg)
            target = client.binding if mismatch == 'definition' else client.host_binding
            target['description'] += ' changed'
            agent = worker.InferenceWorker(cfg, client, provider)
            with self.assertRaises(worker.WorkerError):
                await agent.dispatch(request_id(cfg))
            self.assertEqual(provider.calls, [])
            self.assertEqual(client.claimed, set())
        client, provider = FakeMCP(cfg), FakeProvider(cfg)
        bound = await worker.InferenceWorker(cfg, client, provider).bind()
        self.assertEqual(bound['processor'], client.pin)
        self.assertEqual(bound['operation'], cfg.operation_binding())
        self.assertEqual(bound['config_sha256'], cfg.config_sha256)

    async def test_dispatch_holds_result_and_stale_settlement_passes_through(self):
        cfg = config()
        client, provider = FakeMCP(cfg), FakeProvider(cfg)
        agent = worker.InferenceWorker(cfg, client, provider)
        prepared = await agent.dispatch(request_id(cfg))
        self.assertEqual(prepared.output, provider.content)
        self.assertEqual(prepared.provider.config_sha256, cfg.config_sha256)
        self.assertEqual(client.completed, [])
        client.fresh = False
        completion = await agent.settle(prepared)
        self.assertFalse(completion['fresh'])
        self.assertEqual(client.completed, [{'request_id': prepared.request_id, 'output': provider.content}])
        self.assertEqual(provider.calls, ['hello'])

    async def test_duplicate_claim_never_calls_provider_again(self):
        cfg = config()
        client, provider = FakeMCP(cfg), FakeProvider(cfg)
        agent = worker.InferenceWorker(cfg, client, provider)
        await agent.dispatch(request_id(cfg))
        with self.assertRaises(worker.WorkerError):
            await agent.dispatch(request_id(cfg))
        self.assertEqual(provider.calls, ['hello'])
        self.assertEqual(client.completed, [])

    async def test_claim_identity_mismatch_never_calls_provider(self):
        cfg = config()
        client, provider = FakeMCP(cfg), FakeProvider(cfg)
        client.bad_claim = True
        with self.assertRaises(worker.WorkerError) as raised:
            await worker.InferenceWorker(cfg, client, provider).dispatch(request_id(cfg))
        self.assertTrue(raised.exception.uncertain)
        self.assertEqual(provider.calls, [])

    async def test_maximum_two_concurrent_provider_calls(self):
        cfg = config()
        client, provider = FakeMCP(cfg), FakeProvider(cfg)
        agent = worker.InferenceWorker(cfg, client, provider, max_concurrency=2)
        await asyncio.gather(*(agent.run(request_id(cfg, entity=str(index))) for index in range(5)))
        self.assertEqual(provider.max_active, 2)
        self.assertEqual(len(client.completed), 5)

    async def test_provider_failure_never_completes_or_retries(self):
        cfg = config()
        client, provider = FakeMCP(cfg), FakeProvider(cfg, fail=True)
        with self.assertRaises(worker.WorkerError) as raised:
            await worker.InferenceWorker(cfg, client, provider).run(request_id(cfg))
        self.assertTrue(raised.exception.uncertain)
        self.assertEqual(provider.calls, ['hello'])
        self.assertEqual(client.completed, [])

    async def test_settlement_rechecks_bound_instance_and_validates_exact_output(self):
        cfg = config()
        for content in ('bad\x00output', 'x' * (1024 * 1024)):
            client, provider = FakeMCP(cfg), FakeProvider(cfg, content=content)
            agent = worker.InferenceWorker(cfg, client, provider)
            with self.assertRaises(worker.WorkerError):
                prepared = await agent.dispatch(request_id(cfg))
                await agent.settle(prepared)
            self.assertEqual(client.completed, [])
        client, provider = FakeMCP(cfg), FakeProvider(cfg)
        agent = worker.InferenceWorker(cfg, client, provider)
        prepared = await agent.dispatch(request_id(cfg))
        client.instance_id = 'replacement'
        with self.assertRaises(worker.WorkerError):
            await agent.settle(prepared)
        self.assertEqual(client.completed, [])

    async def test_cli_retains_exact_prepared_result_when_settlement_fails(self):
        cfg = config()
        client, provider = FakeMCP(cfg), FakeProvider(cfg)
        client.fail_settle = True
        identity = request_id(cfg)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config.json'
            path.write_text(json.dumps(cfg.public_dict()))
            args = worker.build_parser().parse_args(['--config', str(path), '--descriptor', 'descriptor',
                '--binary', 'server', '--request-id', identity, '--receipt', str(Path(directory) / 'receipt.json')])
            with patch.object(worker.MCPPipe, 'connect', return_value=client), patch.object(worker, 'ProviderClient', return_value=provider):
                receipt = await worker._cli(args)
        self.assertFalse(receipt['passed'])
        self.assertEqual(receipt['configuration'], cfg.public_dict())
        self.assertEqual(len(receipt['worker_source_sha256']), 64)
        result = receipt['results'][0]
        self.assertFalse(result['completed'])
        self.assertTrue(result['uncertain'])
        self.assertEqual(result['prepared']['request_id'], identity)
        self.assertEqual(result['prepared']['provider']['content'], provider.content)
        self.assertEqual(provider.calls, ['hello'])
        self.assertEqual(client.completed, [])


class Input:
    def __init__(self, callback=None):
        self.data, self.callback = bytearray(), callback
    def write(self, value):
        self.data.extend(value)
        if self.callback:
            self.callback(bytes(value))
    async def drain(self):
        await asyncio.sleep(0)
    def close(self):
        pass
    async def wait_closed(self):
        pass


class Process:
    def __init__(self, response=b'', returncode=0, hang=False):
        self.stdin, self.stdout = Input(), asyncio.StreamReader()
        self.pid, self.returncode, self.killed = 987654321, None if hang else returncode, False
        self.stdout.feed_data(response)
        if not hang:
            self.stdout.feed_eof()
    async def wait(self):
        return self.returncode
    def kill(self):
        self.killed, self.returncode = True, -9


class BridgeProcess:
    def __init__(self):
        self.responses, self.requests = [], []
        self.pending, self.max_pending, self.returncode = 0, 0, 0
        self.stdin, self.stdout = Input(self.respond), self
    def respond(self, payload):
        request = json.loads(payload)
        self.requests.append(request)
        self.pending += 1
        self.max_pending = max(self.max_pending, self.pending)
        result = ({'serverInfo': {'name': 'lemmalog-ddlog'}} if request['method'] == 'initialize'
                  else {'isError': False, 'content': [{'type': 'text', 'text': json.dumps(request['params']['arguments'])}]})
        self.responses.append(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}).encode() + b'\n')
    async def readline(self):
        await asyncio.sleep(0.005)
        self.pending -= 1
        return self.responses.pop(0)
    async def wait(self):
        return self.returncode
    def kill(self):
        self.returncode = -9


def provider_response():
    return {'id': 'response-one', 'model': 'test-model',
            'choices': [{'message': {'role': 'assistant', 'content': 'answer', 'reasoning_content': 'PRIVATE_REASONING'}, 'finish_reason': 'stop'}],
            'usage': {'prompt_tokens': 3, 'completion_tokens': 4, 'total_tokens': 7}}


class TransportTests(unittest.IsolatedAsyncioTestCase):
    async def test_recorded_real_settlements_pass_current_validator_without_provider_calls(self):
        evidence = ROOT / 'docs/evidence/registered-inference/rpc.jsonl'
        completions = [row for line in evidence.read_text().splitlines()
                       if (row := json.loads(line))['tool'] == 'complete_agent_request' and 'result' in row]
        self.assertEqual(len(completions), 4)
        self.assertEqual({row['result']['duplicate'] for row in completions}, {False, True})
        self.assertEqual({row['result']['fresh'] for row in completions}, {False, True})
        for row in completions:
            envelope = {'jsonrpc': '2.0', 'id': 1, 'result': {
                'isError': False, 'content': [{'type': 'text', 'text': json.dumps(row['result'])}]}}
            process = Process(json.dumps(envelope).encode() + b'\n')
            client = worker.MCPPipe(process)
            self.assertEqual(await client.call('complete_agent_request', row['arguments']), row['result'])
            await client.close()

    async def test_malformed_successful_settlement_poisons_pipe_without_replay(self):
        identity = request_id(config())
        accepted = {'request_id': identity, 'fresh': True, 'duplicate': False,
                    'transaction': {'version': 1, 'deltas': ''}}
        malformed = [{}, [], dict(accepted, request_id='different'),
                     {key: value for key, value in accepted.items() if key != 'fresh'},
                     dict(accepted, fresh=1), dict(accepted, duplicate=0),
                     {key: value for key, value in accepted.items() if key != 'transaction'},
                     dict(accepted, transaction={}), dict(accepted, transaction={'version': True, 'deltas': ''}),
                     dict(accepted, transaction={'version': 1, 'deltas': None}),
                     dict(accepted, transaction={'version': 0, 'deltas': ''}),
                     dict(accepted, duplicate=True)]
        for value in malformed:
            with self.subTest(value=value):
                envelope = {'jsonrpc': '2.0', 'id': 1, 'result': {
                    'isError': False, 'content': [{'type': 'text', 'text': json.dumps(value)}]}}
                process = Process(json.dumps(envelope).encode() + b'\n')
                client = worker.MCPPipe(process)
                with self.assertRaises(worker.WorkerError) as raised:
                    await client.call('complete_agent_request', {'request_id': identity, 'output': 'answer'})
                self.assertTrue(raised.exception.uncertain)
                previous = bytes(process.stdin.data)
                with self.assertRaises(worker.WorkerError):
                    await client.call('instance_info')
                self.assertEqual(bytes(process.stdin.data), previous)
                await client.close()

    async def test_valid_new_and_duplicate_settlement_shapes_are_accepted(self):
        identity = request_id(config())
        for value in [{'request_id': identity, 'fresh': True, 'duplicate': False,
                       'transaction': {'version': 1, 'deltas': ''}},
                      {'request_id': identity, 'fresh': False, 'duplicate': True}]:
            envelope = {'jsonrpc': '2.0', 'id': 1, 'result': {
                'isError': False, 'content': [{'type': 'text', 'text': json.dumps(value)}]}}
            process = Process(json.dumps(envelope).encode() + b'\n')
            client = worker.MCPPipe(process)
            self.assertEqual(await client.call('complete_agent_request',
                             {'request_id': identity, 'output': 'answer'}), value)
            await client.close()

    async def test_modal_cli_uses_stdin_profile_and_sanitized_result(self):
        cfg = config()
        raw = json.dumps(provider_response()).encode()
        process = Process(raw)
        with patch.object(worker.asyncio, 'create_subprocess_exec', return_value=process) as spawn:
            result = await worker.ProviderClient(cfg, modal_bin='/approved/modal').infer('payload')
        args, kwargs = spawn.call_args
        self.assertEqual(args, ('/approved/modal', 'curl', '--silent', '--show-error', '--fail-with-body',
                               '--max-time', '600', '-H', 'Content-Type: application/json', '--data-binary', '@-', cfg.endpoint))
        self.assertNotIn('env', kwargs)
        self.assertEqual(kwargs['start_new_session'], worker.os.name == 'posix')
        request = json.loads(process.stdin.data)
        self.assertEqual(request['reasoning'], {'enabled': True})
        self.assertEqual(request['reasoning_effort'], 'high')
        self.assertFalse(request['stream'])
        self.assertEqual(request['messages'][-1], {'role': 'user', 'content': 'payload'})
        self.assertEqual(result.request_sha256, hashlib.sha256(process.stdin.data).hexdigest())
        self.assertEqual(result.response_sha256, hashlib.sha256(raw).hexdigest())
        self.assertNotIn('PRIVATE_REASONING', repr(result))

    async def test_provider_nonzero_timeout_and_response_cap_are_uncertain_once(self):
        cfg = config()
        leader_exited = Process(hang=True)
        leader_exited.returncode = 0
        for process, options in [(Process(b'PRIVATE_SERVER_ERROR', 22), {}),
                                 (Process(hang=True), {'timeout': 0.01}),
                                 (leader_exited, {'timeout': 0.01}),
                                 (Process(b'x' * 100), {'response_cap': 50})]:
            with patch.object(worker.asyncio, 'create_subprocess_exec', return_value=process) as spawn, patch.object(worker.os, 'killpg') as group_kill:
                with self.assertRaises(worker.WorkerError) as raised:
                    await worker.ProviderClient(cfg, **options).infer('payload')
            if options and worker.os.name == 'posix':
                group_kill.assert_called_once_with(process.pid, worker.signal.SIGKILL)
            self.assertTrue(raised.exception.uncertain)
            self.assertNotIn('PRIVATE_SERVER_ERROR', str(raised.exception))
            self.assertEqual(spawn.call_count, 1)

    async def test_provider_invalid_schema_and_output_cap_never_settle(self):
        cfg = config()
        bad = provider_response()
        bad['choices'][0]['message']['content'] = None
        incomplete = provider_response()
        incomplete['choices'][0]['finish_reason'] = 'length'
        for raw, options in [(json.dumps(bad).encode(), {}), (json.dumps(incomplete).encode(), {}),
                             (json.dumps(provider_response()).encode(), {'max_output_bytes': 2})]:
            with patch.object(worker.asyncio, 'create_subprocess_exec', return_value=Process(raw)):
                with self.assertRaises(worker.WorkerError) as raised:
                    await worker.ProviderClient(cfg, **options).infer('payload')
            self.assertTrue(raised.exception.uncertain)

    async def test_mcp_connect_initializes_and_serializes_independent_rpc_ids(self):
        process = BridgeProcess()
        with patch.object(worker.asyncio, 'create_subprocess_exec', return_value=process) as spawn:
            client = await worker.MCPPipe.connect('/operator/server', '/operator/descriptor')
        self.assertEqual(spawn.call_args.args, ('/operator/server', 'connect', '--descriptor', '/operator/descriptor'))
        results = await asyncio.gather(*(client.call('echo', {'value': value}) for value in range(4)))
        self.assertEqual(results, [{'value': value} for value in range(4)])
        self.assertEqual([request['id'] for request in process.requests], [1, 2, 3, 4, 5])
        self.assertEqual(process.max_pending, 1)
        await client.close()
        self.assertEqual(client.bridge_exit_code, 0)

    async def test_mcp_oversized_request_is_rejected_before_write(self):
        process = Process()
        client = worker.MCPPipe(process)
        with self.assertRaises(worker.WorkerError) as raised:
            await client.call('complete_agent_request', {'request_id': 'id', 'output': 'x' * worker.MCP_REQUEST_LIMIT})
        self.assertFalse(raised.exception.uncertain)
        self.assertEqual(process.stdin.data, b'')
        await client.close()

    async def test_mcp_malformed_identity_or_shape_breaks_pipe_without_replay(self):
        for response in ({'jsonrpc': '2.0', 'id': 99, 'result': {}}, [],
                         {'jsonrpc': '2.0', 'id': True, 'result': {}},
                         {'jsonrpc': '2.0', 'id': 1, 'error': {}},
                         {'jsonrpc': '2.0', 'id': 1, 'result': {}, 'error': {}}):
            process = Process(json.dumps(response).encode() + b'\n')
            client = worker.MCPPipe(process)
            with self.assertRaises(worker.WorkerError) as raised:
                await client.call('claim_agent_request', {'request_id': 'id'})
            self.assertTrue(raised.exception.uncertain)
            previous = bytes(process.stdin.data)
            with self.assertRaises(worker.WorkerError):
                await client.call('claim_agent_request', {'request_id': 'id'})
            self.assertEqual(bytes(process.stdin.data), previous)
            await client.close()

    async def test_mcp_eof_is_uncertain_and_never_reconnects(self):
        process = Process()
        client = worker.MCPPipe(process, timeout=0.1)
        with self.assertRaises(worker.WorkerError) as raised:
            await client.call('claim_agent_request', {'request_id': 'id'})
        self.assertTrue(raised.exception.uncertain)
        previous = bytes(process.stdin.data)
        with self.assertRaises(worker.WorkerError):
            await client.call('claim_agent_request', {'request_id': 'id'})
        self.assertEqual(bytes(process.stdin.data), previous)
        await client.close()


if __name__ == '__main__':
    unittest.main()
