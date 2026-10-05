# Hosted external publication boundaries (local Rust port)

This opt-in library port uses the existing `WorldManager`, admission records,
managed checkpoint format, pinned restore, and native process controls. It adds
no world owner, provider, application scheduler, or analytical catalog. Existing
synchronous `admit_inputs` semantics and its serialized responses remain intact.
The new operations are Rust embedding methods; this slice does not add Python,
MCP, socket, or stdio verbs for them.

An embedding application such as Archetype declares public outputs, translates
frozen typed rows into its own analytical tables, publishes a final cut, verifies
that visibility, and confirms its exact receipt. DDlog never calls that publisher
and does not understand Components, Arrow, Daft, Iceberg, or application ticks.
Confirmation is trusted host evidence, not independent external-catalog validation.

## Policy and admission

`WorldDefinition.external_publication` is `Option<ExternalPublicationPolicy>`.
It defaults to `None` and is omitted in old serialized definitions. Existing Rust
struct literals must explicitly add `external_publication: None`; this is an
additive wire change but a Rust source compatibility change.

```rust,ignore
ExternalPublicationPolicy {
    namespace: "archetype".into(),
    outputs: vec!["position".into()],
    max_rows: 1_000_000,
    max_bytes: 64 * 1024 * 1024,
}
```

The immutable policy is persisted at creation, before activation. It selects
1–64 distinct public output names, never inputs or private relations. Creation
rejects test/scenario worlds, registered-operation programs and imported native
operators. The initial port supports checkpointable pure programs/compositions.
Maximum rows are per selected output; maximum bytes are the sum of the final
JSON row-array blobs, including empty arrays. Limits cannot exceed one million
rows/output or 64 MiB across outputs. The separate native checkpoint retains its
existing 64 MiB limit. Namespace identifiers follow the existing bounded token
contract.

`admit_boundary_async(BoundaryAdmission) -> Result<Value, String>` takes existing
`AdmitInputs` and a `PublicationBinding {context, parent_receipt_sha256}`. Context
is opaque application JSON (64 KiB, maximum depth 32); it can bind analytical
world/run, intended tick and component schemas. `effect` and `worker_id` must be
absent in this slice. Existing limits of 1,000 input changes and 256 KiB apply.
Validate the complete batch, public ports, types, checkpoint eligibility and
analytical parent before native mutation. The analytical parent must equal the
last acknowledged external receipt identity, or `None` on a fresh world.

Request identity includes the full admission, binding and immutable policy.
The `BoundaryKey` contains world ID, originating generation, admission key and
request digest. An exact repeat is lookup-only, including after restart or a
later boundary. Changed content under that generation/key rejects. No mutation
or provider work is replayed by a duplicate request.

The existing admission record and pending-boundary key are persisted together
before a native job starts. An uncertain intent-write error retains admission
blocking in memory; it does not authorize apply. Ordinary `admit_inputs`, generic
mutating/unknown execution, and fresh worker launches reject for policy worlds,
even between cuts. Generic execution permits an explicit read-only allowlist.
Shared activation checks prevent fresh Start or ordinary restore from bypassing
a pending boundary. Initial compilation failure or Stop before any admission may
retry Start; a world with publication history requires explicit bound restore.

## Capture and inspection

The async job temporarily owns that world's existing `ProgramInstance`; the
manager retains its existing `ProcessControl`. Native apply/output I/O does not
hold an embedding manager mutex. Other worlds, status, and world stop remain
callable. Shutdown control locks do not span native I/O, filesystem persistence,
or joins. No external publisher callback runs under the manager.

After one acknowledged native apply, the job captures selected complete outputs
and a separate managed checkpoint at the same revision. Each row blob is a JSON
array of typed row arrays. Fields are ordered native `int`/`string` types; zero-row
outputs have an explicit schema, zero count and an empty-array blob. Outputs are
full snapshots, not deltas. Native state and transport failures never become an
empty successful snapshot.

The `FrozenManifest` binds:

- exact key, immutable policy, application context and prior external parent;
- acknowledged revision and each output's name, types, row count, byte size and SHA;
- checkpoint blob size/SHA and the existing managed checkpoint receipt;
- processor/dependency pins, public relation mapping, source/lowering identity,
  original native generation and activation-time build provenance in that receipt.

The managed checkpoint metadata binds the key, policy and context, before a
final manifest digest exists. The external receipt later binds the manifest.
No circular content identity is introduced. Objects are file/directory-synced
before the immutable final frozen manifest. Retrying capture can reuse only
identical bytes at a retained object identity.

Use existing `admission_status(AdmissionQuery)` for progress and manifest lookup.
Its response adds `boundary: {key, manifest, external_receipt}` only for these
admissions. States are `pending`, `applied_but_unpublished`, `uncertain`, `frozen`,
and `published`. `frozen` means capture is complete and external publication is
pending. It does not mean an analytical cut is visible. `applied_revision` is
reported only when native acknowledgement or complete frozen evidence establishes
it. `status.external_publication` exposes pending identity, acknowledged head and
whether a native job is active. Native lifecycle and publication state are separate.

A completion-state persistence failure retains the barrier and returns inspectable
status/evidence with the error. Dirty persistence is retried by ordinary status
handling; a failed retry does not hide external-world inspection. Failure is not
converted into publication success.

`read_boundary_blob(FrozenBlobRead) -> Result<FrozenBlobPage, String>` accepts the
exact key, a digest from its manifest, byte offset and page limit (1..=4 MiB). It
returns owned bytes and an optional next offset. It validates the retained blob's
size/digest, never queries live native output, and accepts no caller filesystem
path. Blobs remain readable after native Stop and owner reopen. Consumers must
complete every page and validate their own assembled objects before publication.
This first local implementation validates the complete blob on each page request;
it makes no indexed-read latency guarantee.

`retry_boundary_freeze_async(BoundaryKey)` retries capture only for the exact
still-live, healthy, acknowledged generation/revision with no competing job. It
never reapplies inputs. Duplicate admission does not start capture. If native
acknowledgement is uncertain or that revision is unavailable, the world stays
blocked. Stop, timeout and dropped waiters cannot clear that state. Administrative
disposition of unresolved native outcomes is deliberately outside this slice.

## Publication acknowledgement and restart

`confirm_boundary_published(BoundaryKey, ExternalReceipt)` requires:

```text
frozen_manifest_sha256 = SHA256(canonical JSON FrozenManifest)
receipt_sha256 = SHA256(canonical JSON {
    "frozen_manifest_sha256": frozen_manifest_sha256,
    "receipt": application_receipt
})
```

Canonical JSON uses `serde_json::Value` object-key ordering and compact encoding,
as with existing request digests. Receipt JSON is limited to 1 MiB and depth 32.
The manifest digest participates in the fenced receipt identity: repeating an
opaque application payload cannot alias two different cuts. The application
verifies actual external publication before making this trusted confirmation.

The exact receipt, new analytical head and barrier release are one durable
world-record update. An identical acknowledgement is safe even while a later cut
is pending. Different receipt content conflicts. On a failed/uncertain write,
the in-memory barrier and exact candidate remain; another receipt cannot steal
that outcome. Retry the same acknowledgement. Reopen observes whichever complete
atomic world record was durable, then reconciles retained frozen evidence.

A durable pending intent is loaded before activation. Complete frozen files can
repair a stale pre-completion world record; no native execution is needed. Missing,
corrupt, or incomplete frozen evidence remains uncertain and blocked. Reopening
never adopts a prior PID, replays an input batch, advances an analytical tick or
chooses a checkpoint automatically. Historical keys keep their original generation.

World Stop and independent owner shutdown retain pending admission and frozen
objects. A forced native interruption can create uncertainty; safe run cancellation
belongs to the caller and stops admission of its next boundary. Teardown cancels
native process groups before joining owned apply/freeze jobs. Retention/GC and
remote or distributed fencing are not supplied by this local port.

## Bound checkpoint import

`restore_boundary_async(BoundCheckpointRestore) -> Result<Value, String>` takes
target world ID, expected target generation, exact frozen manifest, owned
checkpoint bytes, and its exact external receipt. The target must have matching
pin/policy, no active/pending native work or unresolved publication, and a matching
acknowledged analytical head when one already exists.

Validate the checkpoint blob digest, manifest/receipt binding metadata, exact
processor/dependency pins, public schemas, source and lowering before changing
the target head or compiling. Output blob descriptors are validated and bound
by the receipt; their bytes are not supplied or rehashed during import.
The existing pinned restore is checked again during asynchronous installation.
The imported acknowledged head is persisted before activation. A new generation
gets its own activated executable provenance while `restored_from` retains the
original checkpoint's generation and provenance. Recompilation is not a claim of
binary identity. Native inputs are restored; analytical outputs are not used as
an input reconstruction mechanism.

The Rust host may deliberately import into a fresh matching native world, as with
existing managed restore. Analytical same-world resume, latest-head selection,
fork lineage, tenant authorization and server-pinned storage remain the embedding
application's contracts; this port does not infer them from opaque context.

## Evidence and limits

`tests/worlds/boundary.rs` uses the existing simulated native transport. It covers
legacy mutation rejection, complete input preflight, intent-write failure, exact
capture retry without replay, output paging at the exact byte limit, JSON nesting,
parent fencing, receipt conflicts/lost acknowledgements, restart from stale
records, uncertain native apply, independent worlds/status/stop under held native
I/O, inspectable persistence errors, import validation and retraction after restore.
These fixtures do not establish real DDlog evaluation.

The separately invoked
`tests/worlds_native.rs::external_cut_freezes_real_fixed_point_restores_and_retracts`
uses the configured real DDlog compiler. It freezes recursive reachability,
confirms after Stop, reopens/imports the bound checkpoint, retains both activation
identities, and retracts restored inputs to an explicit empty output. Native
acceptance requires `DDLOG_RUNTIME_NATIVE_BUILD` and `--ignored`; an ordinary
ignored result is not native evidence.

Archetype integration is a subsequent local change: consume these frozen typed
blobs with its cut adapter, publish and verify its Iceberg final manifest, confirm
the exact receipt, and prove failure/restart recovery through this hosted owner.
The suspended Archetype duplicate manager must not be shipped. Thin Python and
authenticated API/MCP routes, fork/artifact contracts and consumer migration follow
that integration. No upstream publication or dependent-product release is implied.

## Historical forks

`reserve_fork(ForkRequest)` verifies the exact source manifest, external receipt,
checkpoint bytes and pinned program before child effects. Its bounded manager
catalog binds a request key, source context, destination identity, definition and
source evidence to one child ID. The reservation is fsynced before `world.json`;
reopen materializes a missing child record under that same ID, without activation.
A materialization fence in the same manager catalog is durable before the child can activate. A missing child control record after that fence, or retained generation/boundary evidence without a control record, is corruption and fails closed; it cannot recreate generation zero. Exact retries return the reservation. Changed contents or another request for the
same destination reject. The catalog admits at most 1024 retained reservations.
Destination identity must identify the destination alone, independently of the
source or request key. Trusted embedding chooses both contexts, never paths.

The child's first record carries an unconfirmed fork gate, separate from input
admission state. Ordinary activation and input admission reject until the exact
lineage acknowledgment is durable. `restore_fork_async(reservation, generation,
checkpoint_bytes)` reuses the existing bound restore worker and retains original
checkpoint provenance. Its lost acknowledgment is a lookup at the original
expected generation. A stopped or interrupted candidate requires a new explicit
generation-fenced restore; reservation retry never starts work. Once any child
input admission exists, even unresolved, the historical source cannot replace it.

The embedding stores an immutable analytical origin binding the complete
reservation and unchanged source proof. `ForkReservation::lineage_sha256()` is
its canonical lineage identity. `confirm_fork_lineage` acknowledges that exact
identity only after the source restore completed. It persists readiness before
allowing input. Persistence failure keeps the in-memory gate closed; exact retry
repeats the durable fence. An already-confirmed acknowledgment remains idempotent
after later child inputs or restores. The lineage identity does not replace the
source external receipt: the first child boundary still names that source receipt
as its external parent. Subsequent child receipts advance the ordinary head.

These operations are trusted Rust embedding ports. The embedding must prove
actual analytical origin durability before confirmation; the native manager does
not inspect the embedding's storage. No new worker, input ledger, execution
owner, automatic restore retry or input replay is introduced. Tests inject the
reservation-before-child crash, confirmation write failure and missing catalog,
and exercise cold recovery and source-parent identity. The separately ignored
`historical_fork_restores_selected_fixed_point_and_retracts_independently` test
requires the real compiler. An ignored test is not native acceptance evidence.
