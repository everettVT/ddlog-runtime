#!/usr/bin/env python3
"""SIMULATED compiler used only by shared-host lifecycle tests; never DDlog evidence."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
root = Path(os.environ['FAKE_CONTROL'])
with (root / 'events.jsonl').open('a') as log:
    log.write(json.dumps({'event': 'build', 'pid': os.getpid(), 'pgid': os.getpgrp()}) + '\n')
if (root / 'reject_build').exists():
    sys.exit(91)
if (root / 'hold_build').exists():
    child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(3600)'])
    (root / 'build_child').write_text(str(child.pid))
    while True:
        time.sleep(1)
shutil.copyfile(Path(__file__).with_name('shared_fake_runtime.py'), sys.argv[2])
Path(sys.argv[2]).chmod(0o700)
