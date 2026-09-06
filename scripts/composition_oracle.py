"""Independent finite-set oracle; no compiler, AST, or production imports."""
import re


def expected(facts, threshold=2):
    eligible = {(p, r, f) for p, r, f, severity in facts['findings'] if severity <= threshold}
    selected = {(p, r, f) for p, r, f in eligible if (p, r) in facts['revisions']}
    support = {(p, r, f) for p, r, f, _ in facts['evidence']}
    actionable = {(p, f) for p, r, f in selected if (p, r, f) in support}
    return {'eligible': eligible, 'selected': selected, 'actionable': actionable}


def rows(text, arity):
    result = set()
    for line in text.splitlines():
        match = re.fullmatch(r'R_[A-Za-z0-9_]+\{(.*)\}', line)
        assert match, f'Malformed row: {line}'
        fields = match.group(1).split(', ')
        assert len(fields) == arity, line
        values = []
        for i, field in enumerate(fields):
            part = re.fullmatch(r'\.f' + str(i) + r' = (-?\d+)', field)
            assert part, field
            values.append(int(part.group(1)))
        row = tuple(values)
        assert row not in result, f'Duplicate output row: {line}'
        result.add(row)
    return result


def apply(facts, changes):
    staged = {name: values.copy() for name, values in facts.items()}
    for change in changes:
        values = staged[change['predicate']]
        row = tuple(change['values'])
        if change['op'] == 'insert':
            values.add(row)
        elif change['op'] == 'delete':
            values.discard(row)
        else:
            raise AssertionError('Unknown input operation')
    return staged
