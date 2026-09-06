# Optional external inference worker

`scripts/lemmalog_inference_worker.py` is a Python 3.11+ client of the shared runtime, using only Python's standard library plus an operator-installed, authenticated Modal CLI. The runtime itself does not import or start it.

The worker validates its public configuration against the host operation registry and exact installed program, claims explicitly supplied request IDs, calls the provider outside DDlog, and settles the exact returned string. `InferenceWorker.dispatch()` and `settle()` are separate; `run()` performs both. This is a finite invocation, not a scheduler.

`examples/inference.json` is a placeholder configuration, not a usable provider endpoint. Replace it with your authorized endpoint/model and intended settings. `InferenceConfig.operation_registry()` creates the operator-owned registry file; `operation_binding()` provides the exact binding for the saved program. Canonical configuration JSON determines its SHA-256 operation version.

```sh
python3 scripts/lemmalog_inference_worker.py \
  --config /absolute/config.json \
  --binary /absolute/lemmalog-ddlog-mcp \
  --descriptor /absolute/private/owner.json \
  --request-id 'EXACT_REQUEST_ID_FROM_SUBMIT' \
  --max-concurrency 2 \
  --receipt /absolute/worker-receipt.json
```

Calls run only after explicit claims, with bounded concurrency and no automatic retries. Results are bound to exact operation, entity revision, payload, program pin and instance identity. A late old response may be retained but cannot replace the current revision's output. Duplicate identical settlement is acknowledged; conflicting settlement is rejected.

The worker retains a prepared provider result when settlement cannot be completed, including oversize frames and malformed acknowledgements. Uncertain pipes are invalidated. Requests contain the exact payload, so command-line request IDs and receipts should be handled according to the workload's privacy needs. Provider secrets remain in operator authentication rather than authored programs.

Claims/results are session-local. There is no durable queue, lease expiry, automatic recovery, claimant authentication, or exactly-once provider guarantee. Local process termination does not prove remote cancellation. Same-user clients have equal runtime access. Tests use controlled providers and replay four already-public historical settlement envelopes; extraction verification makes no new provider calls.
