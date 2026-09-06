"""Exact graph partitions independent of Large-Star/Small-Star implementation."""
import importlib.util
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    'connected_components_oracle', ROOT / 'scripts/connected_components_oracle.py')
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)


def facts(vertices=(), edges=()):
    return {'vertices': {(vertex,) for vertex in vertices}, 'edges': set(edges)}


def change(op, predicate, *values):
    return {'op': op, 'predicate': predicate, 'values': list(values)}


class ConnectedComponentsOracle(unittest.TestCase):
    def test_empty_graph_and_isolated_signed_vertices(self):
        self.assertEqual(oracle.expected(facts()), set())
        ids = (-(2**63), -1, 0, 2**63 - 1)
        self.assertEqual(oracle.expected(facts(ids)), {(value, value) for value in ids})

    def test_exact_minimum_labels_for_disconnected_components(self):
        graph = facts((-9, -4, 0, 3, 7, 10, 11, 42, 99),
                      ((-9, -4), (-4, -9), (-4, 0), (0, 0), (3, 7), (7, 3), (10, 11), (99, 99)))
        self.assertEqual(oracle.expected(graph), {
            (-9, -9), (-4, -9), (0, -9), (3, 3), (7, 3),
            (10, 10), (11, 10), (42, 42), (99, 99)})

    def test_edge_only_vertices_self_loop_and_signed_minimum(self):
        self.assertEqual(oracle.expected(facts(edges=((6, -5), (-5, 2), (88, 88)))),
                         {(-5, -5), (2, -5), (6, -5), (88, 88)})

    def test_duplicate_tuple_is_idempotent_reverse_tuple_retains_support(self):
        graph = facts(edges=((1, 2), (2, 1)))
        graph = oracle.apply(graph, [change('insert', 'edges', 1, 2)] * 2)
        graph = oracle.apply(graph, [change('delete', 'edges', 1, 2)])
        self.assertEqual(graph['edges'], {(2, 1)})
        self.assertEqual(oracle.expected(graph), {(1, 1), (2, 1)})
        graph = oracle.apply(graph, [change('delete', 'edges', 2, 1)])
        self.assertEqual(oracle.expected(graph), set())

    def test_bridge_insert_merges_and_delete_splits_exact_partitions(self):
        original = facts((-8, 1, 2, 5, 9), ((-8, 1), (2, 5)))
        merged = oracle.apply(original, [change('insert', 'edges', 1, 5)])
        self.assertEqual(oracle.expected(merged), {(-8, -8), (1, -8), (2, -8), (5, -8), (9, 9)})
        split = oracle.apply(merged, [change('delete', 'edges', 1, 5)])
        self.assertEqual(oracle.expected(split), {(-8, -8), (1, -8), (2, 2), (5, 2), (9, 9)})
        self.assertEqual(split, original)

    def test_deleting_explicit_vertex_keeps_incident_endpoint(self):
        graph = facts((1, 2), ((1, 2),))
        graph = oracle.apply(graph, [change('delete', 'vertices', 1)])
        self.assertEqual(oracle.expected(graph), {(1, 1), (2, 1)})
        graph = oracle.apply(graph, [change('delete', 'edges', 1, 2)])
        self.assertEqual(oracle.expected(graph), {(2, 2)})

    def test_self_loop_endpoint_disappears_only_without_explicit_vertex(self):
        implicit = facts(edges=((-6, -6),))
        self.assertEqual(oracle.expected(implicit), {(-6, -6)})
        self.assertEqual(oracle.expected(oracle.apply(implicit, [change('delete', 'edges', -6, -6)])), set())
        explicit = facts((-6,), ((-6, -6),))
        self.assertEqual(oracle.expected(oracle.apply(explicit, [change('delete', 'edges', -6, -6)])), {(-6, -6)})

    def test_copy_on_apply_and_malformed_batch_preserve_prior_state(self):
        original = facts((1,), ((1, 2),))
        updated = oracle.apply(original, [change('insert', 'vertices', 9)])
        self.assertNotEqual(updated, original)
        self.assertEqual(original, facts((1,), ((1, 2),)))
        with self.assertRaisesRegex(ValueError, 'edges.*2'):
            oracle.apply(original, [change('insert', 'vertices', 9), change('insert', 'edges', 3)])
        self.assertEqual(original, facts((1,), ((1, 2),)))
        for malformed in (change('upsert', 'vertices', 1), change('insert', 'unknown', 1),
                          change('insert', 'vertices', True), change('insert', 'vertices', '1')):
            with self.assertRaises(ValueError):
                oracle.apply(original, [malformed])

    def test_fixture_trace_is_deterministic_and_has_exact_boundary_results(self):
        first = list(oracle.trace())
        self.assertEqual(first, list(oracle.trace()))
        phases = {entry['name']: set(map(tuple, entry['expected_rows'])) for entry in first}
        self.assertEqual(phases['initial'], {
            (-9, -9), (-4, -9), (0, -9), (3, 3), (7, 3),
            (10, 10), (11, 10), (42, 42), (99, 99)})
        self.assertEqual(phases['duplicate_edge'], phases['initial'])
        self.assertEqual(phases['delete_one_orientation'], phases['initial'])
        self.assertEqual(phases['insert_bridge'], {
            (-9, -9), (-4, -9), (0, -9), (3, -9), (7, -9),
            (10, 10), (11, 10), (42, 42), (99, 99)})
        self.assertEqual(phases['delete_forward_bridge'], phases['insert_bridge'])
        self.assertEqual(phases['delete_last_bridge'], phases['initial'])
        self.assertNotIn((42, 42), phases['delete_isolated_vertex'])
        self.assertEqual(phases['restore_isolated_vertex'], phases['initial'])
        self.assertIn((-20, -20), phases['insert_implicit_endpoint'])
        self.assertIn((10, -20), phases['insert_implicit_endpoint'])
        self.assertNotIn((-20, -20), phases['delete_last_implicit_edge'])
        self.assertEqual(phases['delete_last_implicit_edge'], phases['initial'])

    def test_ddlog_label_rows_are_exact_and_reject_malformed_or_duplicate_output(self):
        self.assertEqual(oracle.rows('R_labels{.f0 = -4, .f1 = -9}\nR_labels{.f0 = 42, .f1 = 42}\n'),
                         {(-4, -9), (42, 42)})
        self.assertEqual(oracle.rows(''), set())
        self.assertEqual(oracle.rows('R_components{.f0 = 1, .f1 = 1}\n', 'components'), {(1, 1)})
        invalid = ('R_wrong{.f0 = 1, .f1 = 1}', 'R_labels{.f0 = 1}',
                   'R_labels{.f1 = 1, .f0 = 1}', 'R_labels{.f0 = x, .f1 = 1}',
                   'R_labels{.f0 = 1, .f1 = 1}\nR_labels{.f0 = 1, .f1 = 1}\n')
        for text in invalid:
            with self.assertRaises(ValueError):
                oracle.rows(text)


if __name__ == '__main__':
    unittest.main()
