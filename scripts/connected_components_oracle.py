"""Independent BFS oracle for exact minimum-label connected components.

The vertex universe is explicit vertices plus every edge endpoint. Edges are
undirected for connectivity, while input facts retain directed-tuple set
semantics: inserting an exact duplicate is idempotent, and deleting (u, v)
leaves (v, u) as independent support. Self loops introduce a singleton vertex.

No production/compiler imports or Large-Star/Small-Star steps are used here.
`trace()` is a transport-free acceptance skeleton: each entry supplies one
MCP-shaped mutation batch and the exact expected labels after that batch.
"""
from collections import deque
import json
import re


ARITIES = {'vertices': 1, 'edges': 2}


def _row(predicate, values):
    if predicate not in ARITIES:
        raise ValueError(f'Unknown input predicate: {predicate}')
    try:
        row = tuple(values)
    except TypeError as error:
        raise ValueError(f'{predicate} requires {ARITIES[predicate]} integer fields') from error
    if len(row) != ARITIES[predicate] or any(type(value) is not int for value in row):
        raise ValueError(f'{predicate} requires {ARITIES[predicate]} integer fields')
    return row


def _copy(facts):
    if set(facts) != set(ARITIES):
        raise ValueError('Expected exactly vertices and edges input collections')
    return {name: {_row(name, value) for value in facts[name]} for name in ARITIES}


def expected(facts):
    """Return the exact set of (vertex, minimum component vertex) labels."""
    inputs = _copy(facts)
    adjacent = {vertex: set() for vertex, in inputs['vertices']}
    for left, right in inputs['edges']:
        adjacent.setdefault(left, set()).add(right)
        adjacent.setdefault(right, set()).add(left)
    visited, labels = set(), set()
    for start in sorted(adjacent):
        if start in visited:
            continue
        component = {start}
        visited.add(start)
        frontier = deque([start])
        while frontier:
            vertex = frontier.popleft()
            for neighbor in adjacent[vertex]:
                if neighbor not in visited:
                    visited.add(neighbor)
                    component.add(neighbor)
                    frontier.append(neighbor)
        representative = min(component)
        labels.update((vertex, representative) for vertex in component)
    return labels


def apply(facts, changes):
    """Apply an ordered insert/delete batch to a copy of finite-set inputs."""
    staged = _copy(facts)
    for change in changes:
        if set(change) != {'op', 'predicate', 'values'}:
            raise ValueError('Each change requires exactly op, predicate and values')
        predicate, operation = change['predicate'], change['op']
        row = _row(predicate, change['values'])
        if operation == 'insert':
            staged[predicate].add(row)
        elif operation == 'delete':
            staged[predicate].discard(row)
        else:
            raise ValueError(f'Unknown input operation: {operation}')
    return staged


def rows(text, predicate='labels'):
    """Parse exact two-column DDlog output, rejecting malformed/duplicate rows."""
    pattern = re.compile(r'R_' + re.escape(predicate) + r'\{\.f0 = (-?\d+), \.f1 = (-?\d+)\}')
    result = set()
    for line in text.splitlines():
        match = pattern.fullmatch(line)
        if match is None:
            raise ValueError(f'Malformed {predicate} row: {line}')
        row = tuple(map(int, match.groups()))
        if row in result:
            raise ValueError(f'Duplicate {predicate} row: {line}')
        result.add(row)
    return result


def fixture():
    """Return a fresh deterministic JSON-friendly graph and mutation sequence."""
    def mutation(op, predicate, *values):
        return {'op': op, 'predicate': predicate, 'values': list(values)}

    def phase(name, *changes):
        return {'name': name, 'changes': list(changes)}

    return {
        'vertices': [[vertex] for vertex in (-9, -4, 0, 3, 7, 10, 11, 42, 99)],
        'edges': [[-9, -4], [-4, -9], [-4, 0], [0, 0], [3, 7], [7, 3], [10, 11], [99, 99]],
        'phases': [
            phase('duplicate_edge', mutation('insert', 'edges', -9, -4)),
            phase('delete_one_orientation', mutation('delete', 'edges', -9, -4)),
            phase('restore_orientation', mutation('insert', 'edges', -9, -4)),
            phase('insert_bridge', mutation('insert', 'edges', 0, 3)),
            phase('insert_reverse_bridge', mutation('insert', 'edges', 3, 0)),
            phase('delete_forward_bridge', mutation('delete', 'edges', 0, 3)),
            phase('delete_last_bridge', mutation('delete', 'edges', 3, 0)),
            phase('delete_explicit_self_loop', mutation('delete', 'edges', 99, 99)),
            phase('delete_isolated_vertex', mutation('delete', 'vertices', 42)),
            phase('restore_isolated_vertex', mutation('insert', 'vertices', 42)),
            phase('insert_implicit_endpoint', mutation('insert', 'edges', -20, 11)),
            phase('delete_last_implicit_edge', mutation('delete', 'edges', -20, 11)),
        ],
    }


def trace(scenario=None):
    """Yield mutation batches and exact labels for a future MCP acceptance driver.

    Send each entry's changes in one transaction, query labels, parse with
    rows(), and compare the complete set with expected_rows. This helper does
    not start a host, call MCP, or implement the algorithm under test.
    """
    scenario = fixture() if scenario is None else scenario
    initial = _copy({name: scenario[name] for name in ARITIES})
    changes = [{'op': 'insert', 'predicate': name, 'values': list(value)}
               for name in ARITIES for value in sorted(initial[name])]
    facts = {name: set() for name in ARITIES}
    for phase in [{'name': 'initial', 'changes': changes}, *scenario['phases']]:
        facts = apply(facts, phase['changes'])
        yield {'name': phase['name'], 'changes': phase['changes'],
               'expected_rows': [list(row) for row in sorted(expected(facts))]}


if __name__ == '__main__':
    print(json.dumps({'fixture': fixture(), 'snapshots': list(trace())}, indent=2))
