"""One bounded socket exchange per request; attachment never owns the runtime."""
import json
import math
import socket
import time
import uuid
from pathlib import Path


class AttachedRuntimeClient:
    """One connection per request to an independently owned local runtime.

    A lost reply is never retried. Closing this client cannot close the runtime.
    The descriptor's incarnation fences reconnections to a replacement owner.
    """

    def __init__(self, descriptor, timeout=60, *, on_timing=None):
        self.on_timing = on_timing
        with Path(descriptor).open('rb') as stream:
            data = stream.read(65537)
        if len(data) > 65536:
            raise ValueError('Runtime endpoint descriptor exceeds 64 KiB')
        self.descriptor = json.loads(data)
        if not isinstance(self.descriptor, dict) or self.descriptor.get('schema_version') != 1:
            raise ValueError('Unsupported runtime endpoint descriptor')
        self.socket_path = self.descriptor.get('socket')
        self.owner = self.descriptor.get('owner_incarnation')
        if (not isinstance(self.socket_path, str) or not Path(self.socket_path).is_absolute()
                or not isinstance(self.owner, str) or not 1 <= len(self.owner) <= 128):
            raise ValueError('Invalid runtime endpoint identity or socket path')
        if not math.isfinite(timeout) or timeout <= 0:
            raise ValueError('Runtime timeout must be finite and positive')
        self.timeout = timeout
        self.closed = False

    def request(self, operation, args=None, *, expected_generation=None, expected_revision=None):
        if self.closed:
            raise RuntimeError('Runtime attachment is closed; reconnect explicitly.')
        request_id = uuid.uuid4().hex
        message = {'schema_version': 1, 'request_id': request_id,
                   'owner_incarnation': self.owner, 'operation': operation, 'args': args or {}}
        for name, value in (('expected_generation', expected_generation), ('expected_revision', expected_revision)):
            if value is not None:
                if type(value) is not int or value < 0:
                    raise ValueError(f'{name} must be a nonnegative integer')
                message[name] = value
        wire = json.dumps(message).encode() + b'\n'
        if len(wire) > 1024 * 1024:
            raise RuntimeError('Runtime request exceeds 1 MiB; not sent.')
        sent = False
        started = time.perf_counter()
        timing = {}
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
                deadline = time.monotonic() + self.timeout
                connection.settimeout(self.timeout)
                connection.connect(self.socket_path)
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError('connection deadline exceeded')
                connection.settimeout(remaining)
                sent = True  # sendall can fail after a partial write.
                connection.sendall(wire)
                data = bytearray()
                while not data.endswith(b'\n'):
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise TimeoutError('deadline exceeded')
                    connection.settimeout(remaining)
                    chunk = connection.recv(4096)
                    if not chunk:
                        raise RuntimeError('Runtime closed its response stream')
                    data.extend(chunk)
                    if len(data) > 16 * 1024 * 1024:
                        raise RuntimeError('Runtime response exceeds 16 MiB')
                if data.count(b'\n') != 1:
                    raise RuntimeError('Invalid runtime response framing')
                reply = json.loads(data)
                if (not isinstance(reply, dict) or type(reply.get('ok')) is not bool
                        or reply.get('request_id') != request_id
                        or reply.get('owner_incarnation') != self.owner):
                    raise RuntimeError('Runtime response identity or framing mismatch')
                timing = reply.get("timing") if isinstance(reply.get("timing"), dict) else {}
                return reply
        except (OSError, ValueError, RuntimeError) as error:
            outcome = 'outcome unknown; no retry' if sent else 'not sent'
            raise RuntimeError(f'Runtime request {request_id}: {outcome}: {error}') from error
        finally:
            if self.on_timing is not None:
                # Instrumentation cannot erase an acknowledged mutation result
                # or mask an uncertain transport outcome with a callback error.
                try:
                    self.on_timing(request_id, (time.perf_counter()-started)*1000,
                                   timing.get('queue_ms'), timing.get('execution_ms'))
                except Exception:
                    pass

    def close(self):
        self.closed = True
