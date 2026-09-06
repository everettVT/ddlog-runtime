#!/usr/bin/env python3
"""One fresh native compilation through MCP, followed by insert/query/retract/query."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('artifacts', type=Path, help='New directory retaining source, executable and receipts')
    args = parser.parse_args()
    root = args.artifacts.resolve()
    root.mkdir(parents=True, exist_ok=False)
    requests = [('initialize', {})]
    def call(name, arguments):
        requests.append(('tools/call', {'name': name, 'arguments': arguments}))
    call('lemmalog_install_rules', {'rules': 'visible(X) :- item(X).', 'schemas': {
        'item': {'input': True, 'fields': ['int']},
        'visible': {'input': False, 'fields': ['int']}}})
    call('apply_changes', {'changes': [{'op': 'insert', 'predicate': 'item', 'values': [42]}]})
    call('lemmalog_query', {'predicate': 'visible'})
    call('apply_changes', {'changes': [{'op': 'delete', 'predicate': 'item', 'values': [42]}]})
    call('lemmalog_query', {'predicate': 'visible'})
    wire = ''.join(json.dumps({'jsonrpc': '2.0', 'id': i, 'method': method, 'params': params}) + '\n'
                   for i, (method, params) in enumerate(requests, 1))
    binary = Path(os.environ.get('LEMMALOG_DDLOG_MCP', 'target/debug/lemmalog-ddlog-mcp')).resolve()
    env = dict(os.environ, LEMMALOG_DDLOG_WORKDIR=str(root / 'build'))
    (root / 'requests.jsonl').write_text(wire)
    with (root / 'stderr.log').open('w') as log:
        result = subprocess.run([str(binary)], input=wire, text=True, capture_output=False,
                                stdout=subprocess.PIPE, stderr=log, env=env, timeout=600)
    (root / 'responses.jsonl').write_text(result.stdout)
    result.check_returncode()
    replies = [json.loads(line) for line in result.stdout.splitlines()]
    assert [r['id'] for r in replies] == list(range(1, 7)), replies
    values = []
    for reply in replies[1:]:
        r = reply['result']
        assert not r['isError'], r
        values.append(json.loads(r['content'][0]['text']))
    assert values[0]['backend'] == 'ddlog/differential-dataflow', values[0]
    assert values[2]['rows'] == 'R_visible{.f0 = 42}\n', values[2]
    assert values[4]['rows'] == '', values[4]
    files = {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
             for p in (root / 'build').rglob('*') if p.is_file() and
             (p.suffix == '.dl' or p.name in ('program_cli', 'program', 'runtime'))}
    assert sum(name.endswith('/program_cli') for name in files) == 1, files
    assert sum(name.endswith('/program.dl') for name in files) == 1, files
    receipt = {'passed': True, 'artifact_mode': 'fresh-compilation',
               'host_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
               'checks': ['install', 'insert-visible', 'retract-empty'], 'files': files}
    (root / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(f'PASS: fresh native compile, insert, query, retract, query. Evidence: {root}')


if __name__ == '__main__':
    main()
