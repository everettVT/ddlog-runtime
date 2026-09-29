# Managed local Iceberg (version 1)

Build the owner with `--features iceberg` using Rust 1.95. At startup pass
`--storage-profile /absolute/profile.json`. The trusted same-user regular file
must not be group/world writable and is loaded once:

```json
{"schema_version":1,"profile":{"name":"local","root":"/absolute/private/iceberg","timeout_ms":30000}}
```

There is one profile, one cooperative catalog publisher and one pending staging
or publication job per owner. Restores use the existing world startup lifecycle.
The owner locks the profile root. It derives `catalog.sqlite`,
`warehouse/table` and immutable `warehouse/objects/<receipt-id>.parquet` locations;
no request supplies a catalog path or object URI. Timeout defaults to 30 seconds
and must be 1–120000 ms. The local v3 unpartitioned table is `runtime.checkpoints`
with `commit.retry.num-retries=0`. Configuration is startup-only, and JSON/worker
operations still work without this feature or profile.

## Wire

The existing request/response and attachment envelopes remain unchanged.

- `checkpoint_stage {id,expected_generation,expected_revision,profile}` requires
  the expected healthy running world. It freezes the existing Backend checkpoint
  bytes and ProgramInstance identity under the owner lock, saves publication
  intent, then stages Parquet outside the lock. It returns
  `{schema_version:1,id,receipt_id,status:"staging"}`.
- `checkpoint_status {id,receipt_id}` returns
  `{schema_version:1,receipt,status,availability,error}`. Wait for `staged` before
  requesting publication. Staging never makes a catalog snapshot visible.
- `checkpoint_publish {id,receipt}` requires the exact retained staged receipt
  (or its exact published form). It returns the same ticket shape with
  `status:"publishing"`; poll for `published`. An explicit retry reconciles that
  same immutable object using catalog readback. It never commits input changes.
- `restore {id,receipt}` uses the exact **published** receipt. Existing stopped /
  created / failed / interrupted gates apply. It returns a new starting generation
  immediately. Catalog IO, validation and native compilation run off the owner
  lock; failure never activates a partial backend. Poll the world's lifecycle.
  Ordinary Start remains fresh. Cross-world restore requires the identical pin.

World `status.persistence.checkpoints` includes the same entries. Iceberg states
are `staging`, `staged`, `publishing`, `published`, `uncertain` (invalid manifests
are `unavailable`). `availability:"catalog_unverified"` explicitly means status
has not queried the catalog/object. Restore and explicit publication reconciliation
verify real storage. A timeout/cancellation/error may leave uploaded bytes or a
committed snapshot, so the owner retains its exact receipt and error. Interrupted
staging/publication is marked uncertain at startup, never automatically adopted.
A stage interrupted before its staged receipt was retained may leave an orphan;
it cannot be implicitly restaged or published.

## Exact Iceberg receipt

JSON version-1 receipts are unchanged. The Iceberg receipt keeps the same
`schema_version:1`, `receipt_id`, `program`, `origin`, `checkpoint_sha256` and
`published_at_unix_ms` fields, with `format:"iceberg"` and this additional object:

```json
{"storage":{
  "profile":"local",
  "profile_sha256":"<normalized local profile identity>",
  "table":["runtime","checkpoints"],
  "table_uuid":"<Iceberg table UUID>",
  "object_uri":"file:///absolute/private/iceberg/warehouse/objects/<id>.parquet",
  "object_sha256":"<Parquet bytes digest>",
  "envelope_sha256":"<complete encoded Backend checkpoint digest>",
  "snapshot_id":null
}}
```

At `staged`, snapshot ID and publication time are null. At `published`, they name
exact catalog readback and its first confirmed publication time. `snapshot_id`
is a decimal **string**, never a JSON number: Iceberg's 64-bit IDs exceed browser
integer precision. Preserve the string exactly. Table UUID and
object/envelope hashes may be null during initial staging intent only. Keep the
entire returned receipt opaque/exact for publish and restore. The owner's private
manifest also retains the engine's Avro descriptor; it is never accepted from a
client. Profile identity prevents receipts from silently selecting another store.

`checkpoint_sha256` remains the inner Backend state digest, while
`storage.envelope_sha256` hashes the entire encoded JSON envelope. These digests
are deliberately distinct. Program pins/dependencies, actual build/lowering,
public interfaces, origin revision and effect metadata use the same validation
and hosted fresh-backend restoration as managed JSON.
Capture ingestion keeps its existing asynchronous polling cadence; a world may
be running briefly before `inspection.state` becomes `available`.

## Visibility and ownership

`status.persistence.iceberg` has `schema_version:1`, `status` (`configured`,
`not_configured` or feature `unavailable`), `profile`, `publication_ack:
"catalog_visibility"`, `published_revision`, `error`, and `job` (null or
`{id,generation,receipt_id,cancel_requested}`). It also states
`admission_durability:"json"`. The root `persistence.persisted_revision` retains
its JSON durability meaning. JSON checkpoints and `admit_inputs` continue to use
file/directory-synced JSON independently; an Iceberg publication never grants
effect dispatch authority.

Stop cancels a world's storage job and hosted restore; owner shutdown also cancels
storage work. Cancellation/deadlines do not roll back already dispatched IO.
Completion only records the original immutable publication and cannot replace a
newer world's lifecycle, revision, effects or restore identity. Native queries and
other worlds stay available while storage work waits. No hard latency guarantee
is made for synchronous filesystem calls, input freeze or operating-system IO.

This slice is local SQLite/local FileIO, whole-program checkpoint transport with
the existing 64 MiB format and 128 MiB snapshot-scan limits. Catalog visibility is
not power-loss certification. No remote R2 proof, analytical per-relation tables,
extra Arrow buffering, distributed fencing, automatic garbage collection or
provider retry is implied.

## Focused verification

`tests/managed_iceberg.rs` uses real local SQLite, Parquet and catalog readback.
Its four default tests cover exact receipt reconciliation across owner restart,
unpublished mutation exclusion, tampered bytes, JSON effect-fence preservation,
catalog deadlines, pending-publication acknowledgment, cancellation across newer
generations, store locks, missing catalog and the executable's async wire.

The opt-in `native_managed_iceberg_composition_recovery` test compiles a pinned
composition plus another native world, then restores into a fresh backend. It
checks public outputs, empty inputs, subsequent update/retraction, capture and
new PID ownership. Another world's status/query are measured while a real SQLite
exclusive lock blocks publication. The test uses only fresh temporary stores.

Build the host test with Rust 1.95 and `--features iceberg` **before** sourcing
the native compiler environment. Invoke that compiled test executable with
`DDLOG_RUNTIME_NATIVE_BUILD` pointing to the supported build driver, an isolated
native `CARGO_TARGET_DIR`, and `--exact --ignored --nocapture`. Optional
`DDLOG_ICEBERG_EVIDENCE` records exact native receipts and measurements. This is
local composition recovery proof; the assembled HHMM/Observer acceptance and
remote storage gates remain separate.
