#!/usr/bin/env python3
"""Actual JSON-RPC + compiler + persistent Differential runtime integration test.
Requires the trusted DDlog build-driver environment documented in docs/ddlog.md.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import shlex

binary = os.environ.get('LEMMALOG_DDLOG_MCP', 'target/debug/lemmalog-ddlog-mcp')
receipts = []
schemas = {'finding': {'input': True, 'fields': ['int', 'int', 'int']},
           'current': {'input': True, 'fields': ['int']},
           'actionable': {'input': False, 'fields': ['int', 'int']}}
fixture = tempfile.TemporaryDirectory(prefix='ddlog-build-failure-')
failure = Path(fixture.name)/'fail'
driver = Path(fixture.name)/'driver.sh'
driver.write_text('#!/bin/sh\nif [ -f '+shlex.quote(str(failure))+' ]; then exit 1; fi\nexec '+shlex.quote(os.environ['LEMMALOG_DDLOG_BUILD'])+' "$@"\n')
driver.chmod(0o755)
env = dict(os.environ, LEMMALOG_DDLOG_BUILD=str(driver))
p = subprocess.Popen([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env)
def rpc(method, params):
    request = {'jsonrpc': '2.0', 'id': len(receipts)+1, 'method': method, 'params': params}
    p.stdin.write(json.dumps(request)+'\n'); p.stdin.flush()
    line = p.stdout.readline()
    assert line, 'MCP server exited'
    response = json.loads(line)
    receipts.append({'request': request, 'response': response})
    return response['result']
def call(name, args, error=False):
    r = rpc('tools/call', {'name': name, 'arguments': args})
    assert r['isError'] == error, r
    return r if error else json.loads(r['content'][0]['text'])
def install(n):
    return call('lemmalog_install_rules', {'rules': f'actionable(P,F) :- finding(P,F,S), current(P), S =< {n}.', 'schemas': schemas})
def apply(op, predicate, values):
    return call('apply_changes', {'changes': [{'op': op, 'predicate': predicate, 'values': values}]})
try:
    rpc('initialize', {})
    assert len(rpc('tools/list', {})['tools']) == 4
    assert install(2)['backend'] == 'ddlog/differential-dataflow'
    apply('insert', 'finding', [1, 10, 1])
    assert call('lemmalog_query', {'predicate': 'actionable'})['rows'] == ''
    delta = apply('insert', 'current', [1])['deltas']
    assert 'R_actionable{.f0 = 1, .f1 = 10}: +1' in delta
    assert '.v_S = 1' in call('lemmalog_why', {'rule': 0})['bindings']
    assert apply('insert', 'current', [1])['deltas'] == ''
    apply('insert', 'finding', [1, 10, 2])
    delta = apply('delete', 'finding', [1, 10, 2])['deltas']
    assert 'R_actionable' not in delta  # another supporting binding remains
    delta = apply('delete', 'finding', [1, 10, 1])['deltas']
    assert 'R_actionable{.f0 = 1, .f1 = 10}: -1' in delta
    assert call('lemmalog_why', {'rule': 0})['bindings'] == ''
    apply('insert', 'finding', [1, 11, 3])
    assert call('lemmalog_query', {'predicate': 'actionable'})['rows'] == ''
    assert install(3)['replayed_facts'] == 2
    assert '.f1 = 11' in call('lemmalog_query', {'predicate': 'actionable'})['rows']
    failure.touch()
    call('lemmalog_install_rules', {'rules': 'actionable(P,F) :- finding(P,F,S), current(P), S =< 4.', 'schemas': schemas}, error=True)
    failure.unlink()
    assert '.v_S = 3' in call('lemmalog_why', {'rule': 0})['bindings']
    call('lemmalog_install_rules', {'rules': 'actionable(P,F) :- !finding(P,F,S).', 'schemas': schemas}, error=True)
    call('apply_changes', {'changes': [{'op': 'insert', 'predicate': 'finding', 'values': [1, 12, 'bad']}]}, error=True)
    assert '.f1 = 11' in call('lemmalog_query', {'predicate': 'actionable'})['rows']
    assert '.v_S = 3' in call('lemmalog_why', {'rule': 0})['bindings']
    print('PASS: real MCP -> Lemmalog AST -> DDlog -> Differential/Timely; join, insert, delete, duplicate, witnesses, recompile/replay, rejected mutations')
finally:
    Path(os.environ.get('DDLOG_RECEIPTS', '/tmp/lemmalog-ddlog-receipts.json')).write_text(json.dumps(receipts, indent=2))
    p.stdin.close()
    p.wait(timeout=10)
    fixture.cleanup()
