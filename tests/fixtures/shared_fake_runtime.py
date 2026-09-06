#!/usr/bin/env python3
"""SIMULATED command/fence runtime. Only pipe/failure ownership is under test."""
import json
import os
from pathlib import Path
import sys
import time
root = Path(os.environ['FAKE_CONTROL'])
def event(name):
    with (root / 'events.jsonl').open('a') as log:
        log.write(json.dumps({'event': name, 'pid': os.getpid(), 'pgid': os.getpgrp()}) + '\n')
event('runtime')
facts, staged = set(), set()
for line in sys.stdin:
    command = line.strip()
    if command == 'start;':
        staged = set(facts)
    elif command.startswith(('insert R_source(', 'delete R_source(')):
        value = json.loads(command[command.index('(') + 1:-2])
        if command.startswith('insert'):
            staged.add(value)
        else:
            staged.discard(value)
    elif command.startswith('commit'):
        facts = set(staged)
        if command == 'commit dump_changes;':
            event('mutation')
            (root / 'mutation_entered').touch()
            while (root / 'hold_mutation').exists():
                time.sleep(0.01)
            if (root / 'fail_mutation').exists():
                os._exit(42)  # Lost completion fence after committing simulated state.
    elif command == 'dump R_echo;':
        event('query')
        if (root / 'huge_output').exists():
            print('x' * (5 * 1024 * 1024), flush=True)
        elif (root / 'slow_output').exists():
            print('x' * (2 * 1024 * 1024), flush=True)
        else:
            for value in sorted(facts):
                print('R_echo{.f0 = ' + json.dumps(value) + '}', flush=True)
    elif command.startswith('echo '):
        print(command[5:-1], flush=True)
