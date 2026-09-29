# Managed JSON checkpoint contract (version 1)

`ddlog-worlds` exposes `checkpoint` and `restore` through its existing stdio and
Unix-socket request dispatcher. No new owner, provider, or worker is created.
`src/world_persistence.rs` owns publication, receipt validation, availability and
world-record writes. `ProgramInstance` shares pure-program preparation between
install and restore, including exact registry validation and public-port admission.
`Backend` remains the authority for checkpoint format and acknowledged inputs.

## Requests and receipts

- `checkpoint {"id":"<world-id>"}` requires a healthy running instance, with no
  pending lifecycle operation. It freezes the current committed revision and
  returns the receipt object below only after the object and publication record
  have been file-synced, renamed and directory-synced. This first slice performs
  bounded synchronous JSON capture/publication (64 MiB format limit); no native
  compilation or output scan is involved. There is no latency guarantee for disk IO.
- `restore {"id":"<target-world-id>","receipt":<exact receipt object>}` requires
  `created`, `stopped`, `failed` or `interrupted`, with no live instance or pending
  start/restore. It verifies the stored receipt and checkpoint, records a new
  generation as `starting`, and compiles/replays asynchronously into a fresh
  Backend. Poll `status` or `inventory`; `running` confirms successful activation,
  while `failed` carries the error. `stop` cancels this compiler/replay through the
  same hosted process control as Start. A failed restore never activates partial
  inputs. Test worlds restore without re-running scenarios.
- A different created world may restore a receipt from this owner's private store
  only when its exact processor pin is identical. Other stores, client filesystem
  paths, path traversal and symlinked objects/directories are rejected.
- `start {"id":"..."}` always starts fresh. It never selects a receipt implicitly.
  The stored publication history remains visible after fresh Start.

Library entrypoints are `WorldManager::checkpoint(id) -> Result<Value, String>`
and `WorldManager::restore_async(id, &receipt) -> Result<Value, String>`.
Requests retain the existing `{operation,args}` / `{ok,result|error}` envelope.
There is no automatic command retry or publication retry operation in this slice.
After an error or a lost response, inspect the retained publications; do not assume
that no file was published. Repeating Checkpoint makes a new immutable publication.

A receipt is an opaque exact-match object, not a filename:

```json
{
  "schema_version": 1,
  "format": "json",
  "receipt_id": "<relative opaque numeric-and-dash ID>",
  "program": {
    "processor": {"processor_id": "processor_...", "version": "sha256:..."},
    "dependencies": {"<dependency-key>": {"processor_id": "processor_...", "version": "sha256:..."}},
    "public_relations": [{"name": "edge", "input": true, "fields": ["int", "int"], "physical": "Input_edge"}],
    "lowering_version": 2,
    "source_sha256": "<actual lowered-source digest>"
  },
  "origin": {
    "world_id": "<origin world>",
    "generation": 1,
    "revision": 2,
    "program_version": 1,
    "build": {
      "runtime": {"commit": "<host commit or null>", "dirty": true, "crate_version": "0.1.0", "schema_version": 1},
      "native_sha256": "<executable digest captured at activation>",
      "source_sha256": "<actual lowered-source digest>",
      "lowering_version": 2,
      "program_version": 1
    }
  },
  "checkpoint_sha256": "<Backend checkpoint state digest>",
  "published_at_unix_ms": 1790000000000
}
```

`dependencies` is empty for a plain program; a composition carries its exact
transitive dependency map. `public_relations` contains all public inputs/outputs
and their actual physical mapping. Origin identity and program identity are also
integrity-bound in the Backend checkpoint's application metadata. The caller
cannot substitute metadata or use the receipt to supply executable source.

The immutable registry record and dependency closure are validated again. Restore
re-lowers the pin under the receipt's lowering version and compares complete source,
schemas, dependency identity and public interface before invoking the compiler.
Changing a dependency's current pointer does not change the pinned version.
Missing, corrupt or incompatible pins fail closed. Registered-operation programs
and imported native operators remain unsupported by JSON format 1; checkpoint
reports the existing format error. All inputs, including empty relations, survive.

Restoration recompiles under the operator's current configured driver. The receipt
records the origin executable; it does not require byte-identical recompilation or
certify equivalence of arbitrary compiler versions. The new instance exposes its
own `instance.build`, generation and PID, plus the restored committed revision.
Its capture path and full-detail/rotation options belong to the new generation.
The digest detects corruption, not a malicious operator able to rewrite the entire
private store. Keep the configured native toolchain trusted and pinned.

## Status, durability and compatibility

`status.persistence` (also in summary inventory) contains `schema_version: 1` and:

- `status: configured|error`, `format: json`, `coverage: whole_pure_program_inputs`,
  and `error` for a checkpoint/store failure. `configured` means the managed JSON
  store is enabled, not that every program is checkpointable or every receipt valid.
- `committed_revision`: current live revision, or null without a healthy instance.
- `persisted_revision`: highest retained published boundary for this generation,
  or the boundary explicitly restored into it. Fresh Start does not inherit the
  previous generation's persisted revision. Check individual availability before
  presenting a receipt as a recovery choice.
- `checkpoints`: retained entries `{receipt,status,availability,error}`. Publication
  phases are `staging`, `staged`, `published`, `failed`, `uncertain`; unreadable
  entries are `unavailable` and retain their `receipt_id` plus diagnostic. Only
  `published` accepts restore. Incomplete publication has no confirmed publication
  time; it is never promoted on owner restart.
- `availability: present_unverified|missing_or_invalid` describes the object file.
  An unreadable manifest has `availability: invalid`. Summary polling checks file
  presence/type/size and reads bounded manifests; it does not hash checkpoint
  bodies, query native rows, or resolve registry dependencies. `verification`
  states that Restore performs those integrity and pin checks before compilation.
- `restore_requested`: the exact receipt selected for this generation, including
  failed/cancelled attempts; `restored_from` is set only after successful replay.
  Lifecycle history retains these identities even after a later fresh Start.

The private layout is
`<BUILD_ROOT>/<origin-world>/checkpoints/<receipt-id>/{checkpoint.json,publication.json}`.
Clients cannot choose any of those paths. Receipt manifests are capped at 1 MiB.
The publication record is the authority for a durable receipt, independent of a
subsequent `world.json` write; a failed world-record write cannot erase publication
history. Publication errors after a possible rename are uncertain and retained
where storage permits. Missing objects and malformed manifests do not prevent
world inventory or lifecycle operations.

Old `world.json` records deserialize with empty persistence state. Old Backend
checkpoint bytes and registry fixtures are unchanged and remain validated by the
existing library API. A raw historical Backend checkpoint has no managed receipt,
so it cannot be passed directly to this protocol. No historical world starts or
restores automatically. World records, registry records and the managed receipt
store must all be retained together; build caches are not a replacement for them.

Coverage describes whole pure program inputs, not analytical storage per relation.
The [managed worker contract](managed-workers.md) adds generic durable admissions
and effect fences using these same receipts. Its optional effect metadata is
integrity-bound within the checkpoint; the receipt schema remains version 1.
The optional [managed Iceberg extension](managed-iceberg.md) reuses this format
and validation with separate asynchronous stage/publication operations. JSON
`persisted_revision` remains independent of Iceberg `published_revision`.
Neither contract adds automatic provider recovery.

## Verification and integration risk

`tests/worlds/persistence.rs` uses a simulated native transport to cover publication
failure, owner restart, tampering, missing objects/dependencies, private-path
rejection, composition admission, explicit fresh Start, failed build/replay, hosted
cancellation and observer options. `tests/worlds_stdio.rs` verifies the wire verbs.
`tests/worlds_native.rs::managed_json_restore_recomputes_public_state_and_preserves_process_capture`
requires `DDLOG_RUNTIME_NATIVE_BUILD`: it compiles both a pure program and a
composition, restores the exact public rows into a fresh owner, omits an unpublished
mutation, verifies empty inputs and a subsequent insertion/retraction, checks
process ownership/capture, and proves Start remains fresh. It makes no provider call.

World start/restore workers do **not** serialize build-driver calls across worlds.
The bundled driver's `build-native-artifact.py` uses content-keyed native targets
and holds the key lock across input verification, Cargo build and publication.
Custom drivers sharing targets must prevent both generated-package freshness
collisions and build/copy races; a copy lock alone does not solve freshness.
Runtime acceptance uses a private native target separate from other lanes.
