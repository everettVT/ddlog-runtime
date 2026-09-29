#!/usr/bin/env python3
"""Trusted test worker only; no provider, prompt, or application integration."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time

launch = json.load(sys.stdin)  # also proves owner closes stdin after the envelope
root = Path(os.environ["FIXTURE_ROOT"])
mode = launch["payload"].get("mode", "complete")
(root / (launch["key"] + ".launch.json")).write_text(json.dumps(launch))
if mode in ("hold", "descendant"):
    if mode == "descendant":
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"])
        (root / (launch["key"] + ".child")).write_text(str(child.pid))
    time.sleep(120)
elif mode == "oversized":
    print("x" * 20000, flush=True)
    time.sleep(120)
elif mode == "invalid":
    print('{"prompt":"never expose this output"}', flush=True)
    sys.exit(0)
elif mode == "exit":
    sys.exit(9)
elif mode in ("failed_exit", "completed_exit", "invalid_exit", "unknown_exit"):
    outcome = {"schema_version": 1, "status": "failed", "code": "configuration_invalid"}
    if mode == "completed_exit":
        outcome["status"] = "completed"
    elif mode == "invalid_exit":
        outcome["code"] = "never expose private output"
    elif mode == "unknown_exit":
        outcome["prompt"] = "never expose private output"
    print(json.dumps(outcome), flush=True)
    sys.exit(9)
elif mode == "attached":
    descriptor = json.loads(Path(launch["owner_descriptor"]).read_text())
    sequence = 0

    def call(operation, args):
        global sequence
        sequence += 1
        request = dict(schema_version=1, request_id=str(sequence),
                       owner_incarnation=descriptor["owner_incarnation"],
                       operation=operation, args=args)
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(15)
            client.connect(descriptor["socket"])
            client.sendall(json.dumps(request).encode() + b"\n")
            response = json.loads(client.makefile("rb").readline(16 * 1024 * 1024))
        assert response["ok"], response
        return response["result"]

    common = dict(id=launch["world_id"], expected_generation=launch["generation"],
                  worker_id=launch["worker_id"])
    observed = call("status", {"id":launch["world_id"]})
    revision = observed["revision"]
    evidence = call("read_batch", dict(common, expected_revision=revision,
                    queries=[{"predicate":"source"},{"predicate":"echo"}]))
    assert evidence["revision"] == revision and all(p["complete"] for p in evidence["results"])
    claim = dict(common, expected_revision=revision, admission_key=launch["key"]+".reserve",
                 changes=[{"op":"insert","predicate":"source","values":[10,"reserved"]}],
                 effect={"key":launch["key"],"phase":"reserve"})
    reserved = call("admit_inputs", claim)
    assert reserved["state"] == "durable" and reserved["effect_authorized"]
    replay = call("admit_inputs", claim)
    assert replay["replayed"] and not replay["effect_authorized"]
    (root / (launch["key"] + ".reserved.json")).write_text(json.dumps(reserved))
    # Simulates time outside the owner, without performing any external effect.
    release = root / (launch["key"] + ".release")
    while not release.exists():
        time.sleep(.01)
    settled = call("admit_inputs", dict(common, expected_revision=reserved["applied_revision"],
                admission_key=launch["key"]+".settle",
                changes=[{"op":"delete","predicate":"source","values":[10,"reserved"]},
                         {"op":"insert","predicate":"source","values":[20,"settled"]}],
                effect={"key":launch["key"],"phase":"settle","reservation_key":claim["admission_key"]}))
    assert settled["state"] == "durable" and not settled["effect_authorized"]
    (root / (launch["key"] + ".settled.json")).write_text(json.dumps(settled))
print(json.dumps({"schema_version":1,"status":"completed","code":"fixture_ok"}), flush=True)
