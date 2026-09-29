"""Response identity, no retry, and advisory timing for the shared attachment."""
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'python'))
from ddlog_runtime_client import AttachedRuntimeClient


class TransportTests(unittest.TestCase):
    def exchange(self, modify=lambda reply: None, callback=None, disconnect=False):
        with tempfile.TemporaryDirectory() as directory:
            descriptor = Path(directory) / 'endpoint.json'
            descriptor.write_text(json.dumps({'schema_version':1, 'socket':'/fixture.sock',
                                              'owner_incarnation':'owner'}))
            client = AttachedRuntimeClient(descriptor, on_timing=callback)
            connection = MagicMock()
            def receive(_):
                if disconnect:
                    return b''
                request = json.loads(connection.sendall.call_args.args[0])
                reply = {'ok':True, 'request_id':request['request_id'],
                         'owner_incarnation':'owner', 'result':{'acknowledged':True}}
                modify(reply)
                return json.dumps(reply).encode() + b'\n'
            connection.recv.side_effect = receive
            with patch('ddlog_runtime_client.transport.socket.socket') as factory:
                factory.return_value.__enter__.return_value = connection
                try:
                    return client.request('admit_inputs', {'id':'world'})
                finally:
                    self.assertEqual(factory.call_count, 1)
                    self.assertEqual(connection.sendall.call_count, 1)

    def test_advisory_timing_cannot_discard_acknowledgment(self):
        def broken_callback(*_):
            raise RuntimeError('telemetry unavailable')
        reply = self.exchange(lambda reply: reply.update(timing='malformed'), broken_callback)
        self.assertTrue(reply['result']['acknowledged'])

    def test_mismatched_response_identity_is_unknown_and_not_retried(self):
        for key in ['request_id', 'owner_incarnation']:
            with self.subTest(key=key), self.assertRaisesRegex(RuntimeError, 'outcome unknown; no retry'):
                self.exchange(lambda reply: reply.update({key:'different'}))

    def test_timing_failure_does_not_mask_lost_reply(self):
        def broken_callback(*_):
            raise ValueError('metrics failure')
        with self.assertRaisesRegex(RuntimeError, 'outcome unknown; no retry.*closed'):
            self.exchange(callback=broken_callback, disconnect=True)


if __name__ == '__main__':
    unittest.main()
