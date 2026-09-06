"""Independent current-result/set-join oracle; never calls a provider or engine."""
import json
import re


def expected(current, responses, owners):
    reviewed = {(entity, revision, responses[request])
                for entity, (revision, request) in current.items() if request in responses}
    routed = {(entity, revision, output, owner)
              for entity, revision, output in reviewed
              for owner in owners.get(entity, set())}
    return {'reviewed': reviewed, 'routed': routed}


def rows(text, predicate):
    """Parse CLI record fields as JSON scalars, preserving escaped provider text."""
    result = set()
    decoder = json.JSONDecoder()
    for line in text.splitlines():
        prefix = 'R_' + predicate + '{'
        if not line.startswith(prefix) or not line.endswith('}'):
            raise ValueError('Unexpected output record: ' + line[:100])
        rest, values = line[len(prefix):-1], []
        while rest:
            match = re.match(r'\s*\.f(\d+)\s*=\s*', rest)
            if not match or int(match[1]) != len(values):
                raise ValueError('Unexpected output field: ' + rest[:100])
            value, end = decoder.raw_decode(rest[match.end():])
            values.append(value)
            rest = rest[match.end() + end:].lstrip()
            if rest:
                if not rest.startswith(','):
                    raise ValueError('Missing record field separator')
                rest = rest[1:]
        result.add(tuple(values))
    return result
