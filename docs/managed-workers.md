# Managed worker and durable admission contract (version 1)

This is the generic runtime seam for an independently attached external worker.
Runtime owns admission, checkpoint publication and local child lifetime. The
worker owns application schemas, prompts, providers, tools and reconciliation.
There is no second daemon, automatic restart or automatic external-call retry.

## Startup profiles and launch

Launch the existing owner with `--listen ENDPOINT.json --worker-profiles PROFILES.json`.
The profile file is an absolute, same-user regular file, not group/world writable,
read once at startup; no protocol verb changes profiles. Its schema is:

```json
{"schema_version":1,"profiles":{"example":{
  "argv":["/absolute/python","-m","application.worker"],
  "env":{"APPLICATION_MODE":"fixture"},
  "config_ref":"/absolute/operator/config.json",
  "allowed_processors":[{"processor_id":"processor_...","version":"sha256:..."}],
  "max_concurrent_workers":4,
  "timeout_ms":300000
}}}
```

`env` defaults to empty, `config_ref` to null. The first argv element must be
absolute. Runtime never evaluates a shell or substitutes caller payload into argv
or env. The profile config reference is passed to the child, never read as provider
configuration by Runtime. Profiles have 1–64 concurrent children each, a 1 ms–1 hour
lifetime limit, and exact allowed processor pins. No profile means no worker launch.

- `worker_start {id,expected_generation,profile,key,payload}`: requires a running
  world and the profile's exact pin. `key` is a bounded opaque identifier, unique
  across profiles for that world/generation. Identical requests return the existing
  record without relaunching, even after completion/failure. Different payload or
  profile for that key conflicts. Payload is at most 64 KiB JSON.
- `worker_status {id,generation?,worker_id?}`: returns retained records, optionally
  selecting a generation/worker. `worker_stop {id,expected_generation,worker_id}`
  cancels only that child process group. World Stop and owner shutdown cancel all
  owned children, including descendants. Reopening never adopts an old PID.

Stdin receives exactly one newline-terminated launch envelope then EOF:

```json
{"schema_version":1,"world_id":"...","generation":1,"worker_id":"...",
 "profile":"example","key":"run-123","processor":{"processor_id":"...","version":"..."},
 "owner_descriptor":"/absolute/ENDPOINT.json","config_ref":"/absolute/operator/config.json",
 "payload":{}}
```

The child uses the existing descriptor/owner-incarnation socket protocol. It must
carry `worker_id` on read/admission requests to fence cancellation/late results.
Runtime accepts at most 16 KiB stdout containing one JSON outcome:
`{"schema_version":1,"status":"completed"|"failed","code":"optional_identifier"}`.
A zero exit plus a valid completed outcome establishes local completion only.
Nonzero exit, malformed/oversized output and timeouts fail. Stderr is discarded.
A valid bounded `status:"failed"` outcome and its code are retained even on a
nonzero exit; a nonzero exit can never establish `completed`.
Status exposes identity, profile hash, PID, resources, timestamps, state, exit code,
validated outcome code and generic errors; it never includes argv/env/config,
payload, stdout text or application prompts. States are starting/running/stopping,
completed/failed/stopped/interrupted. No worker or effect is retried automatically.
Process-group cleanup covers World Stop, worker cancellation and graceful owner
shutdown (including TERM/INT). An uncatchable owner kill or machine failure cannot
run cleanup; reopening marks unfinished records interrupted and never signals or
adopts recorded PIDs, which may have been reused.

## Consistent reads and fenced durable writes

`read_batch {id,expected_generation,expected_revision,worker_id?,queries:[
{predicate,max_rows?,continuation?}]}` performs 1–16 public-relation reads under the
same owner lock, returning `{schema_version:1,id,generation,revision,results:[...]}`.
Total requested rows are at most 1000 and the response is capped at 4 MiB. Results
use the existing query_rows page shape and continuations. Incomplete pages are
explicit; the caller must not treat them as complete evidence. Every later batch
must use the same generation/revision. Input continuation tokens are additionally
fenced by those mandatory arguments. The write must carry that evidence revision.
Obtain the first generation/revision from `status`. A stale read fence can restart
the entire evidence read at a newly observed revision; discard earlier pages.
This read-only retry does not authorize mutation or an external-call retry.

`admit_inputs {id,expected_generation,expected_revision,admission_key,changes,
worker_id?,effect?}` performs validation, one plain native input commit and managed
JSON checkpoint publication while holding the existing exclusive owner lock.
Changes use the existing public typed input contract; no output deltas are read.
The request is at most 256 KiB and 1000 changes. Keys are bounded identifiers.

Result fields: `{schema_version:1,id,generation,admission_key,state,applied_revision,
receipt,error,replayed,effect_authorized,publication}`. Outcomes:

- `durable`: native acknowledgment and durable publication succeeded. Receipt is
  the exact managed checkpoint object; applied_revision names its committed inputs.
- `not_applied`: validation/fencing failed before a native transaction.
- `applied_but_unpublished`: native commit acknowledged but no durable publication
  acknowledgment can be returned. `publication: uncertain` means files may exist;
  this is never permission to call a provider.
- `uncertain`: native acknowledgment or final durable admission-record write was
  lost. Reconcile explicitly. No automatic mutation/effect retry is allowed.

`admission_status {id,generation,admission_key}` reads a retained outcome without
re-executing. Once an execution intent is recorded, repeating the exact admission
request is lookup-only; changing its contents conflicts. Preflight rejections
do not consume a key or promise a retained status record. All lookups return
`replayed:true,effect_authorized:false`.
Historical/pending records survive owner restart; uncompleted intent is uncertain.
Intent is persisted before mutation, without retaining caller changes/prompts.
At most 4096 admissions and 1024 effects are retained per world; exhausting either
fails before mutation and requires an explicit new-world/reconciliation decision.

## Optional generic effect reservation and settlement

For an external call use `effect:{key,phase:"reserve"}` on the admission that writes
its application's claim. The fresh response must be `durable` and
`effect_authorized:true` before the worker makes that call. The reservation key is
that admission's `admission_key`. Duplicate effect keys cannot reserve again,
including through another worker or after checkpoint restoration. Lost admission
responses never authorize automatic provider retry.

Settle through another fenced admission with
`effect:{key,phase:"settle",reservation_key:"<original admission key>"}` and the
application's result changes. The matching reservation must belong to this world,
generation and worker and must still be reserved. This prevents duplicate settlement
and results from old generations/stopped workers. Unrelated revisions may advance;
refresh consistent evidence and use the current revision, retaining the exact
reservation identity. No provider is recalled when CAS fails.

Generic effect metadata is integrity-bound alongside inputs in Backend checkpoint
metadata, without changing the version-1 receipt shape. Restore retains reservations
and settlements but never grants dispatch authority or changes their origin fences.
Ordinary fresh Start clears execution state. Explicit restoration of a checkpoint
from before an effect is not an exactly-once guarantee; application/operator
reconciliation remains necessary. No generic retry/release/reassignment operation
is included in this slice.

The trusted worker is not a hostile-client sandbox: same-user socket clients retain
operator access. The worker must use these fenced APIs and carry its worker ID;
legacy execute remains available to existing operator clients.
