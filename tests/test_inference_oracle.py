import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('inference_oracle', Path(__file__).resolve().parents[1] / 'scripts/inference_oracle.py')
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)


class InferenceOracleTests(unittest.TestCase):
    def test_stale_response_excluded_and_owner_join_independent(self):
        current = {'item': (2, 'new')}
        responses = {'old': 'obsolete', 'new': 'current'}
        self.assertEqual(oracle.expected(current, responses, {'item': {'a', 'b'}}), {
            'reviewed': {('item', 2, 'current')},
            'routed': {('item', 2, 'current', 'a'), ('item', 2, 'current', 'b')}})
        self.assertEqual(oracle.expected(current, {'old': 'obsolete'}, {})['reviewed'], set())

    def test_exact_escaped_multiline_content(self):
        self.assertEqual(oracle.rows('R_reviewed{.f0 = "item", .f1 = 2, .f2 = "a\\n\\\"b\\\""}\n', 'reviewed'),
                         {('item', 2, 'a\n"b"')})

    def test_unexpected_record_rejected(self):
        for text in ['header:', 'R_reviewed{.f1 = 2}', 'R_reviewed{.f0 = 2 broken}']:
            with self.assertRaises(ValueError):
                oracle.rows(text, 'reviewed')


if __name__ == '__main__':
    unittest.main()
