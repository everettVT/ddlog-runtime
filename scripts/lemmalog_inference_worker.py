#!/usr/bin/env python3
"""Finite external inference worker for existing shared-host claim/complete tools.

Configuration and results contain no credentials. Modal's existing operator
profile authenticates the one subprocess call. Claims are session-local, and
uncertain outcomes are never retried or automatically completed. Terminating
the local subprocess does not promise cancellation at the provider.
"""
import argparse
import asyncio
from dataclasses import asdict, dataclass
import hashlib
import json
import math
import os
from pathlib import Path
import re
import signal
import sys
from urllib.parse import urlsplit

MCP_REQUEST_LIMIT = 1024 * 1024
MCP_RESPONSE_LIMIT = 4 * 1024 * 1024
DEFAULT_MODAL = 'modal'


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False, allow_nan=False).encode('utf-8')


def digest(value):
    return hashlib.sha256(value).hexdigest()


class WorkerError(RuntimeError):
    def __init__(self, message, *, uncertain=False):
        super().__init__(message)
        self.uncertain = uncertain


@dataclass(frozen=True)
class InferenceConfig:
    format_version: int
    operation: str
    endpoint: str
    model: str
    system_prompt: str
    max_tokens: int = 32768
    temperature: float = 0.3
    top_p: float = 0.9
    reasoning_effort: str = 'high'
    reasoning_enabled: bool = True

    def __post_init__(self):
        if type(self.format_version) is not int or self.format_version != 1:
            raise ValueError('Unsupported config format_version; use integer 1')
        if not isinstance(self.operation, str) or not re.fullmatch(r'[A-Za-z][A-Za-z0-9_.:-]{0,127}', self.operation):
            raise ValueError('operation must be a nonempty bounded operation name')
        if not isinstance(self.endpoint, str) or len(self.endpoint) > 2048 or any(character.isspace() or ord(character) < 32 or ord(character) == 127 for character in self.endpoint):
            raise ValueError('endpoint must be an absolute HTTPS URL without credentials, query or fragment')
        try:
            endpoint = urlsplit(self.endpoint)
            valid = (endpoint.scheme == 'https' and endpoint.hostname and endpoint.username is None
                     and endpoint.password is None and not endpoint.query and not endpoint.fragment
                     and '?' not in self.endpoint and '#' not in self.endpoint)
            endpoint.port
        except ValueError:
            valid = False
        if not valid:
            raise ValueError('endpoint must be an absolute HTTPS URL without credentials, query or fragment')
        for name, value, limit in [('model', self.model, 512), ('system_prompt', self.system_prompt, 65536)]:
            if not isinstance(value, str) or not value.strip() or len(value.encode('utf-8')) > limit:
                raise ValueError(f'{name} must be a nonempty bounded string')
        if type(self.max_tokens) is not int or not 1 <= self.max_tokens <= 32768:
            raise ValueError('max_tokens must be an integer from 1 through 32768')
        for name, value, minimum, maximum in [('temperature', self.temperature, 0, 2), ('top_p', self.top_p, 0, 1)]:
            if type(value) not in (int, float) or not math.isfinite(value) or not minimum <= value <= maximum:
                raise ValueError(f'{name} is outside its supported finite range')
        if self.top_p == 0:
            raise ValueError('top_p must be greater than zero')
        if self.reasoning_effort not in ('low', 'medium', 'high'):
            raise ValueError('reasoning_effort must be low, medium or high')
        if type(self.reasoning_enabled) is not bool:
            raise ValueError('reasoning_enabled must be a boolean')

    @classmethod
    def from_dict(cls, value):
        required = {'format_version', 'operation', 'endpoint', 'model', 'system_prompt'}
        allowed = required | {'max_tokens', 'temperature', 'top_p', 'reasoning_effort', 'reasoning_enabled'}
        if not isinstance(value, dict) or set(value) - allowed or required - set(value):
            raise ValueError('Invalid inference config fields; use the supported public fields only, without credentials')
        return cls(**value)

    @classmethod
    def load(cls, path):
        def unique(pairs):
            value = {}
            for key, item in pairs:
                if key in value:
                    raise ValueError('Duplicate inference config field; use each public field once')
                value[key] = item
            return value
        try:
            value = json.loads(Path(path).read_text(), object_pairs_hook=unique)
        except (OSError, UnicodeError, json.JSONDecodeError) as error:
            raise ValueError('Cannot read inference config JSON; correct the operator config file') from error
        return cls.from_dict(value)

    def public_dict(self):
        return asdict(self)

    @property
    def config_sha256(self):
        return digest(canonical(self.public_dict()))

    def operation_binding(self):
        return {'name': self.operation, 'version': 'sha256:' + self.config_sha256,
                'description': f'External inference model {self.model}; public config sha256:{self.config_sha256}'}

    def operation_registry(self):
        binding = self.operation_binding()
        return {binding['name']: {key: binding[key] for key in ('version', 'description')}}


@dataclass(frozen=True)
class ProviderResult:
    content: str
    model: str
    id: str
    usage: dict
    finish_reason: str
    request_sha256: str
    response_sha256: str
    config_sha256: str


@dataclass(frozen=True)
class PreparedResult:
    request_id: str
    operation: str
    operation_version: str
    instance_id: str
    processor_id: str
    processor_version: str
    provider: ProviderResult

    @property
    def output(self):
        return self.provider.content


class SettlementError(WorkerError):
    """A complete provider result remains available for explicit reconciliation."""
    def __init__(self, message, prepared, *, uncertain):
        super().__init__(message, uncertain=uncertain)
        self.prepared = prepared


async def _stop_process(process, *, process_group=False):
    if process_group and os.name == 'posix':
        # Modal's curl command creates a curl child in this new, worker-owned
        # session. Bound local cleanup covers both; remote cancellation is unknown.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    if process.returncode is None:
        try:
            process.kill()
        except ProcessLookupError:
            pass
    try:
        await asyncio.wait_for(process.wait(), timeout=5)
    except asyncio.TimeoutError:
        raise WorkerError('Local child did not exit within the cleanup bound; inspect the process before continuing', uncertain=True)


async def _send(process, payload):
    process.stdin.write(payload)
    await process.stdin.drain()
    process.stdin.close()
    try:
        await process.stdin.wait_closed()
    except (BrokenPipeError, ConnectionResetError):
        pass


class ProviderClient:
    def __init__(self, config, modal_bin=DEFAULT_MODAL, timeout=630, response_cap=MCP_RESPONSE_LIMIT,
                 max_output_bytes=MCP_REQUEST_LIMIT):
        if (type(timeout) not in (int, float) or not math.isfinite(timeout) or timeout <= 0
                or type(response_cap) is not int or not 1 <= response_cap <= MCP_RESPONSE_LIMIT
                or type(max_output_bytes) is not int or not 1 <= max_output_bytes <= MCP_REQUEST_LIMIT):
            raise ValueError('Provider timeout must be finite and positive; response/output caps must be bounded positive integer byte counts')
        self.config, self.modal_bin = config, str(modal_bin)
        self.timeout, self.response_cap, self.max_output_bytes = timeout, response_cap, max_output_bytes

    async def infer(self, payload):
        if not isinstance(payload, str):
            raise WorkerError('Inference payload must be a string; inspect the admitted request')
        request = canonical({'model': self.config.model, 'messages': [
            {'role': 'system', 'content': self.config.system_prompt}, {'role': 'user', 'content': payload}],
            'max_tokens': self.config.max_tokens, 'temperature': self.config.temperature,
            'top_p': self.config.top_p, 'reasoning_effort': self.config.reasoning_effort,
            'reasoning': {'enabled': self.config.reasoning_enabled}, 'stream': False})
        if len(request) > 2 * MCP_REQUEST_LIMIT:
            raise WorkerError('Provider request exceeds the bounded input size; reduce the next request before admission')
        arguments = (self.modal_bin, 'curl', '--silent', '--show-error', '--fail-with-body', '--max-time', '600',
                     '-H', 'Content-Type: application/json', '--data-binary', '@-', self.config.endpoint)
        try:
            process = await asyncio.create_subprocess_exec(*arguments, stdin=asyncio.subprocess.PIPE,
                        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
                        start_new_session=os.name == 'posix')
        except OSError as error:
            raise WorkerError('Cannot start approved Modal transport; inspect the operator executable/profile and reconcile the existing claim', uncertain=True) from error

        async def exchange():
            sender = asyncio.create_task(_send(process, request))
            try:
                chunks, size = [], 0
                while True:
                    chunk = await process.stdout.read(65536)
                    if not chunk:
                        break
                    size += len(chunk)
                    if size > self.response_cap:
                        raise WorkerError('Provider response exceeded its bound; outcome uncertain, reconcile the claim and provider before resubmission', uncertain=True)
                    chunks.append(chunk)
                await sender
                return b''.join(chunks), await process.wait()
            finally:
                if not sender.done():
                    sender.cancel()
                await asyncio.gather(sender, return_exceptions=True)

        try:
            raw, returncode = await asyncio.wait_for(exchange(), timeout=self.timeout)
        except (asyncio.TimeoutError, asyncio.CancelledError, OSError) as error:
            await _stop_process(process, process_group=True)
            raise WorkerError('Provider transport interrupted or timed out; outcome uncertain, reconcile the claim/provider and do not automatically retry', uncertain=True) from error
        except WorkerError:
            await _stop_process(process, process_group=True)
            raise
        if returncode != 0:
            raise WorkerError(f'Modal transport exited with status {returncode}; outcome uncertain, inspect provider state and reconcile the existing claim without automatic retry', uncertain=True)
        try:
            response = json.loads(raw)
            choices = response['choices']
            if not isinstance(choices, list) or len(choices) != 1:
                raise ValueError('choices')
            choice = choices[0]
            content, model, identity = choice['message']['content'], response['model'], response['id']
            if not all(isinstance(value, str) and value.strip() for value in (content, model, identity)):
                raise ValueError('content/model/id')
            if choice['finish_reason'] != 'stop':
                raise WorkerError('Provider generation is incomplete or has an unsupported finish reason; no completion sent, inspect the response outcome before a new request', uncertain=True)
            if len(content.encode('utf-8')) > self.max_output_bytes:
                raise WorkerError('Provider content exceeds the output bound; no completion sent, reduce the next request budget', uncertain=True)
            usage = response['usage']
            if not isinstance(usage, dict):
                raise ValueError('usage')
            public_usage = {}
            for key in ('prompt_tokens', 'completion_tokens', 'total_tokens'):
                if key in usage:
                    if type(usage[key]) is not int or usage[key] < 0:
                        raise ValueError('usage count')
                    public_usage[key] = usage[key]
        except (KeyError, TypeError, ValueError, UnicodeError) as error:
            raise WorkerError('Provider response does not match the supported nonempty chat-completion schema; outcome uncertain, inspect provider state before resubmission', uncertain=True) from error
        return ProviderResult(content, model, identity, public_usage, 'stop', digest(request), digest(raw), self.config.config_sha256)


def _frame(sequence, method, parameters):
    return canonical({'jsonrpc': '2.0', 'id': sequence, 'method': method, 'params': parameters}) + b'\n'


class MCPPipe:
    def __init__(self, process, timeout=600):
        if type(timeout) not in (int, float) or not math.isfinite(timeout) or timeout <= 0:
            raise ValueError('MCP timeout must be finite and positive')
        self.process, self.timeout = process, timeout
        self._lock, self._sequence, self._broken = asyncio.Lock(), 0, False
        self._closed, self.bridge_exit_code = False, None

    @classmethod
    async def connect(cls, binary, descriptor, timeout=600):
        try:
            process = await asyncio.create_subprocess_exec(str(binary), 'connect', '--descriptor', str(descriptor),
                stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL, limit=MCP_RESPONSE_LIMIT + 1)
        except OSError as error:
            raise WorkerError('Cannot start the MCP pipe bridge; inspect the binary and shared-instance descriptor') from error
        client = cls(process, timeout)
        try:
            initialized = await client.rpc('initialize', {})
            if not isinstance(initialized, dict) or not isinstance(initialized.get('serverInfo'), dict) or initialized['serverInfo'].get('name') != 'lemmalog-ddlog':
                raise WorkerError('Connected server is not the expected DDlog host; inspect the descriptor')
        except BaseException:
            await client.close()
            raise
        return client

    async def rpc(self, method, parameters):
        async with self._lock:
            if self._broken or self._closed:
                raise WorkerError('MCP pipe is unavailable; reconcile prior outcomes before explicitly connecting again', uncertain=True)
            self._sequence += 1
            payload = _frame(self._sequence, method, parameters)
            if len(payload) > MCP_REQUEST_LIMIT:
                raise WorkerError('Encoded MCP request exceeds 1 MiB; reduce the request/output before submitting it')
            try:
                self.process.stdin.write(payload)
                await asyncio.wait_for(self.process.stdin.drain(), self.timeout)
                raw = await asyncio.wait_for(self.process.stdout.readline(), self.timeout)
                if not raw or not raw.endswith(b'\n') or len(raw) > MCP_RESPONSE_LIMIT:
                    raise ValueError('missing or oversized response')
                response = json.loads(raw)
                if (not isinstance(response, dict) or type(response.get('id')) is not int
                        or response['id'] != self._sequence or response.get('jsonrpc') != '2.0'
                        or ('result' in response) == ('error' in response)):
                    raise ValueError('response identity')
                if 'error' in response and (not isinstance(response['error'], dict)
                        or type(response['error'].get('code')) is not int
                        or not isinstance(response['error'].get('message'), str)):
                    raise ValueError('error envelope')
            except (OSError, ValueError, TypeError, asyncio.TimeoutError, asyncio.CancelledError) as error:
                self._broken = True
                raise WorkerError('MCP response was lost, malformed, or timed out; outcome uncertain, reconcile the host and do not replay the request', uncertain=True) from error
            if 'error' in response:
                raise WorkerError('MCP rejected the protocol request; inspect the server protocol and correct the request')
            if 'result' not in response:
                self._broken = True
                raise WorkerError('MCP response has no result; outcome uncertain, inspect host state without replay', uncertain=True)
            return response['result']

    async def call(self, name, arguments=None):
        result = await self.rpc('tools/call', {'name': name, 'arguments': arguments or {}})
        try:
            text = result['content'][0]['text']
            if not isinstance(text, str):
                raise ValueError('text')
            if result.get('isError', False):
                raise WorkerError(text, uncertain='uncertain' in text.lower())
            value = json.loads(text)
            if name == 'complete_agent_request':
                try:
                    _validate_settlement((arguments or {}).get('request_id'), value)
                except WorkerError:
                    self._broken = True
                    raise
            return value
        except (KeyError, IndexError, TypeError, ValueError) as error:
            self._broken = True
            raise WorkerError('MCP tool returned malformed content; outcome uncertain, inspect host state without replay', uncertain=True) from error

    async def close(self):
        if self._closed:
            return
        self._closed = True
        self.process.stdin.close()
        try:
            self.bridge_exit_code = await asyncio.wait_for(self.process.wait(), timeout=10)
        except asyncio.TimeoutError:
            await _stop_process(self.process)
            self.bridge_exit_code = self.process.returncode
            raise WorkerError('MCP bridge did not close after EOF; inspect prior outcomes, no request was retried', uncertain=True)
        if self.bridge_exit_code != 0:
            raise WorkerError('MCP bridge exited unsuccessfully; reconcile prior outcomes before reconnecting', uncertain=True)

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_):
        await self.close()


def _validate_completion(identity, output):
    if not isinstance(output, str) or not output.strip():
        raise WorkerError('Provider output must be nonempty text; no completion sent, inspect the response', uncertain=True)
    if any(ord(character) < 32 and character not in '\b\t\n\f\r' for character in output):
        raise WorkerError('Provider output contains a control character unsupported by DDlog; no completion sent, inspect the response without altering it silently', uncertain=True)
    try:
        frame = _frame(2**63 - 1, 'tools/call', {'name': 'complete_agent_request',
                       'arguments': {'request_id': identity, 'output': output}})
    except (UnicodeError, ValueError) as error:
        raise WorkerError('Provider output cannot be encoded as the exact UTF-8 completion; inspect the response', uncertain=True) from error
    if len(frame) > MCP_REQUEST_LIMIT:
        raise WorkerError('Exact completion including its request identity exceeds the 1 MiB MCP frame limit; no completion sent, use a smaller future input/output budget', uncertain=True)


def _validate_settlement(identity, result):
    """A transport success alone does not establish completion of this request."""
    cause = None
    if not isinstance(result, dict):
        cause = 'expected a completion object'
    elif result.get('request_id') != identity or not isinstance(identity, str):
        cause = 'request_id is missing or differs from the admitted request'
    elif type(result.get('fresh')) is not bool or type(result.get('duplicate')) is not bool:
        cause = 'fresh and duplicate must both be explicit booleans'
    elif result['duplicate']:
        if 'transaction' in result:
            cause = 'duplicate acknowledgement must not claim a new transaction'
    else:
        transaction = result.get('transaction')
        if (not isinstance(transaction, dict) or type(transaction.get('version')) is not int
                or not 1 <= transaction['version'] <= 2**64 - 1
                or not isinstance(transaction.get('deltas'), str)):
            cause = 'new completion requires a transaction with a positive integer version and string deltas'
    if cause:
        raise WorkerError(f'Malformed completion acknowledgement: {cause}; settlement outcome uncertain. '
                          'Inspect instance_info and agent_request_status and reconcile the retained prepared response '
                          'before deciding on explicit settlement; do not automatically replay the request', uncertain=True)


class InferenceWorker:
    def __init__(self, config, client, provider, max_concurrency=2):
        if type(max_concurrency) is not int or not 1 <= max_concurrency <= 16:
            raise ValueError('max_concurrency must be an integer from 1 through 16')
        self.config, self.client, self.provider = config, client, provider
        self._slots, self._binding_lock = asyncio.Semaphore(max_concurrency), asyncio.Lock()
        self._bound = None

    async def _instance(self):
        info = await self.client.call('instance_info')
        pin = info.get('processor')
        if (info.get('health') != 'ready' or not isinstance(info.get('instance_id'), str)
                or not isinstance(pin, dict) or not all(isinstance(pin.get(key), str) and pin[key] for key in ('processor_id', 'version'))):
            raise WorkerError('Worker requires a ready shared instance pinned to an immutable processor; inspect instance_info and install the intended version first')
        if self._bound and (info['instance_id'] != self._bound['instance_id'] or pin != self._bound['processor']):
            raise WorkerError('Bound instance or processor pin changed; inspect the intended instance and reconcile admitted requests before continuing', uncertain=True)
        return info

    async def bind(self):
        async with self._binding_lock:
            info = await self._instance()
            if self._bound is None:
                pin = info['processor']
                record = await self.client.call('processor_get', dict(pin))
                binding = self.config.operation_binding()
                if ({key: record.get(key) for key in ('processor_id', 'version')} != pin
                        or record.get('definition', {}).get('operation') != binding):
                    raise WorkerError('Pinned processor operation does not match the worker public config hash and binding; inspect processor_get and select the exact matching config before admission')
                registered = await self.client.call('agent_operations')
                matching = [entry for entry in registered.get('operations', []) if entry.get('name') == binding['name']]
                if len(matching) != 1 or any(matching[0].get(key) != value for key, value in binding.items()) or matching[0].get('input') != 'string' or matching[0].get('output') != 'string':
                    raise WorkerError('Host registered operation does not match the worker binding; inspect agent_operations and select matching configuration before admission')
                self._bound = {'instance_id': info['instance_id'], 'processor': dict(pin),
                               'operation': binding, 'config_sha256': self.config.config_sha256}
            return json.loads(json.dumps(self._bound))

    async def dispatch(self, request_id):
        async with self._slots:
            bound = await self.bind()
            claim = await self.client.call('claim_agent_request', {'request_id': request_id})
            try:
                identity = json.loads(request_id)
                expected = [self.config.operation, bound['operation']['version'], claim['entity'], claim['revision'], claim['payload']]
                valid = (identity == expected and claim['request_id'] == request_id and claim['status'] == 'claimed'
                         and claim['operation'] == expected[0] and claim['operation_version'] == expected[1]
                         and isinstance(claim['entity'], str) and bool(claim['entity'])
                         and type(claim['revision']) is int and claim['revision'] >= 0 and isinstance(claim['payload'], str))
            except (KeyError, TypeError, ValueError):
                valid = False
            if not valid:
                raise WorkerError('Admitted request identity or operation version differs from the bound config; inspect the claim and reconcile without invoking the provider', uncertain=True)
            result = await self.provider.infer(claim['payload'])
            if not isinstance(result, ProviderResult) or result.config_sha256 != self.config.config_sha256:
                raise WorkerError('Provider result config identity differs from the admitted binding; no completion sent, reconcile the provider outcome', uncertain=True)
            # Preserve the completed provider result before checking whether its
            # exact text plus request identity can be submitted to the graph.
            return PreparedResult(request_id, self.config.operation, bound['operation']['version'], bound['instance_id'],
                                  bound['processor']['processor_id'], bound['processor']['version'], result)

    async def settle(self, prepared):
        bound = await self.bind()
        if (prepared.instance_id != bound['instance_id'] or prepared.processor_id != bound['processor']['processor_id']
                or prepared.processor_version != bound['processor']['version'] or prepared.operation != self.config.operation
                or prepared.operation_version != bound['operation']['version'] or prepared.provider.config_sha256 != self.config.config_sha256):
            raise WorkerError('Prepared result belongs to a different bound instance/config; inspect the exact pin before explicit settlement', uncertain=True)
        _validate_completion(prepared.request_id, prepared.output)
        completion = await self.client.call('complete_agent_request', {'request_id': prepared.request_id, 'output': prepared.output})
        _validate_settlement(prepared.request_id, completion)
        return completion

    async def run(self, request_id):
        prepared = await self.dispatch(request_id)
        try:
            completion = await self.settle(prepared)
        except Exception as error:
            raise SettlementError(str(error), prepared, uncertain=getattr(error, 'uncertain', True)) from error
        return {'prepared': prepared, 'completion': completion}


class _Parser(argparse.ArgumentParser):
    def error(self, _message):
        self.print_usage(file=sys.stderr)
        self.exit(2, 'Invalid worker arguments; use --help for supported public configuration options.\n')


def build_parser():
    parser = _Parser(description=__doc__)
    parser.add_argument('--config', required=True)
    parser.add_argument('--descriptor', required=True)
    parser.add_argument('--binary', required=True)
    parser.add_argument('--modal-bin', default=DEFAULT_MODAL)
    parser.add_argument('--request-id', action='append', required=True)
    parser.add_argument('--max-concurrency', type=int, default=2)
    parser.add_argument('--receipt', required=True)
    return parser


async def _cli(arguments):
    receipt = {'format_version': 1, 'passed': False, 'results': [],
               'worker_source_sha256': digest(Path(__file__).read_bytes())}
    client = None
    try:
        config = InferenceConfig.load(arguments.config)
        receipt.update(configuration=config.public_dict(), config_sha256=config.config_sha256, operation=config.operation_binding())
        client = await MCPPipe.connect(arguments.binary, arguments.descriptor)
        agent = InferenceWorker(config, client, ProviderClient(config, arguments.modal_bin), arguments.max_concurrency)
        receipt['binding'] = await agent.bind()
        results = await asyncio.gather(*(agent.run(identity) for identity in arguments.request_id), return_exceptions=True)
        for identity, result in zip(arguments.request_id, results):
            entry = {'request_id_sha256': digest(identity.encode('utf-8'))}
            if isinstance(result, BaseException):
                entry.update(error=str(result), uncertain=getattr(result, 'uncertain', False), completed=False)
                if isinstance(result, SettlementError):
                    entry['prepared'] = asdict(result.prepared)
            else:
                prepared = asdict(result['prepared'])
                completion = result['completion']
                entry.update(prepared=prepared, completed=True, settlement={
                    'fresh': completion['fresh'], 'duplicate': completion['duplicate'],
                    'transaction_version': completion.get('transaction', {}).get('version')})
            receipt['results'].append(entry)
        receipt['passed'] = all(entry['completed'] for entry in receipt['results'])
    except (ValueError, WorkerError) as error:
        receipt.update(error=str(error), uncertain=getattr(error, 'uncertain', False))
    finally:
        if client is not None:
            try:
                await client.close()
            except WorkerError as error:
                receipt.update(passed=False, close_error=str(error), uncertain=True)
            receipt['bridge_exit_code'] = client.bridge_exit_code
    return receipt


def main():
    arguments = build_parser().parse_args()
    receipt = asyncio.run(_cli(arguments))
    descriptor = os.open(arguments.receipt, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, 'w') as output:
        json.dump(receipt, output, indent=2)
        output.write('\n')
    return 0 if receipt['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
