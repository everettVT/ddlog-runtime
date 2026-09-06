"""The stdio bridge must forward each direction before either peer closes."""
import json
import os
from pathlib import Path
import queue
import select
import socket
import subprocess
import tempfile
import threading
import unittest


BINARY = os.path.abspath(os.environ.get('LEMMALOG_DDLOG_MCP', 'target/debug/lemmalog-ddlog-mcp'))


@unittest.skipUnless(hasattr(socket, 'AF_UNIX'), 'Unix transport')
class BridgeTransport(unittest.TestCase):
    def test_request_response_does_not_wait_for_eof(self):
        request = b'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n'
        response = b'{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n'
        events = queue.Queue()
        release = threading.Event()
        with tempfile.TemporaryDirectory(prefix='ddlog-bridge-', dir='/tmp') as directory:
            root = Path(directory)
            endpoint = root / 'socket'
            descriptor = root / 'descriptor.json'
            descriptor.write_text(json.dumps({'instance_id': 'transport-test', 'socket': str(endpoint)}))
            descriptor.chmod(0o600)
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
                listener.bind(str(endpoint))
                listener.listen(1)
                listener.settimeout(10)

                def serve():
                    try:
                        with listener.accept()[0] as stream:
                            stream.settimeout(10)
                            with stream.makefile('rb') as reader:
                                handshake = json.loads(reader.readline())
                                assert handshake == {'kind': 'attach', 'instance_id': 'transport-test'}, handshake
                                stream.sendall(b'{"attached":true,"instance_id":"transport-test"}\n')
                                received = reader.readline()
                                events.put(('request', received))
                                stream.sendall(response)
                                # Keep the host connection open while the client awaits its response.
                                release.wait(10)
                    except Exception as exc:
                        events.put(('error', repr(exc)))

                server = threading.Thread(target=serve, daemon=True)
                server.start()
                with tempfile.TemporaryFile() as errors:
                    bridge = subprocess.Popen([BINARY, 'connect', '--descriptor', str(descriptor)],
                                              stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                              stderr=errors, bufsize=0)
                    try:
                        bridge.stdin.write(request)
                        event, value = events.get(timeout=12)
                        self.assertEqual((event, value), ('request', request), 'Bridge did not forward stdin to the host')
                        ready, _, _ = select.select([bridge.stdout], [], [], 3)
                        self.assertTrue(ready, 'Host received the request and sent a response, but bridge stdout waited for EOF')
                        received = bridge.stdout.readline()
                        self.assertEqual(received, response)
                        self.assertIsNone(bridge.poll(), 'Bridge must remain open for the next request')
                    finally:
                        release.set()
                        bridge.stdin.close()
                        try:
                            bridge.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            bridge.kill()
                            bridge.wait(timeout=5)
                        bridge.stdout.close()
                        server.join(timeout=12)
                        errors.seek(0)
                        detail = errors.read().decode(errors='replace')
                        if detail:
                            print('Bridge stderr:', detail, flush=True)


if __name__ == '__main__':
    unittest.main()
