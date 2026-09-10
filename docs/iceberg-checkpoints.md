# Runtime-owned Iceberg checkpoints

The optional `iceberg` feature stores the runtime's complete format-1 state in
immutable Parquet and publishes it through an operator-provided Iceberg catalog.
It requires Rust 1.95 and pins an Apache Iceberg git revision with Arrow/Parquet
59; the default lightweight runtime and existing JSON checkpoint API remain.

`Backend::stage_iceberg_checkpoint(table, publication_id, object_uri, metadata)`
freezes all acknowledged inputs, generated source, schemas, logical revision,
program version and opaque application metadata using the runtime's own snapshot
API. It writes and rereads Parquet through the table FileIO without committing a
catalog snapshot. Retain the returned `StagedCheckpoint` durably before delaying
publication; it contains descriptors, object and checkpoint digests and identity.
An uncertain staging reply leaves a possible orphan: never overwrite that URI.
Use a fresh object URI or recover the retained receipt. There is no automatic
orphan adoption, garbage collection or write-ahead log.

`iceberg_checkpoint::publish(catalog, table_id, receipt)` validates the receipt
and object, reconstructs a minimal descriptor from the verified file (supplied
metrics are rejected), then publishes the descriptor through Iceberg fast append. It reloads
the catalog and verifies a pinned snapshot before acknowledging publication.
Retry the identical receipt after an uncertain reply; an already visible exact
publication is adopted, and changed publication-ID reuse is rejected. The
catalog is the visibility authority. The dedicated v3 table must use the supplied
schema, no partitioning, and `commit.retry.num-retries=0` so application
reconciliation owns uncertain/conflicting outcomes.

`Backend::restore_iceberg_checkpoint(catalog, table_id, receipt)` reads that
publication's pinned Iceberg snapshot, verifies immutable bytes and payload, and
uses the same validated candidate activation as local checkpoint restore. It
requires a fresh backend. Derived arrangements are recomputed. The application
does not reconstruct state through a parallel mutation ledger.

This version transports one full checkpoint envelope per Parquet row, not
individual application facts for analytical queries. The checkpoint includes
its complete typed relation inventory (including empty inputs). The 64 MiB
format-1 cap, pure-program restriction, and lack of imported-native-operator or
external-inference recovery remain. Snapshot scans have an explicit 128 MiB
aggregate limit and may require deliberate retention/compaction as history grows.

## Ownership and durability

One cooperative publisher owns a checkpoint table and allocates immutable object
URIs. Distributed publisher fencing, concurrent same-ID publication, snapshot
expiration and multi-table atomicity are not implemented. No implicit latest
checkpoint is selected; callers retain an exact receipt. A local SQLite catalog
can coordinate a local owner while its table metadata and Parquet live on object
storage; that does not make the SQLite catalog itself remotely recoverable.
Catalog publication is distinct from power-loss certification or external-effect
exactly-once execution. Native transaction success and successful object upload
are both earlier boundaries than verified catalog publication.

## Acceptance

`examples/iceberg_recovery.rs` runs `stage`, `publish` and `restore` in separate
processes. It compiles a real recursive DDlog reachability program, stages its
state, applies an unpublished mutation, publishes in a new process, retries the
receipt and rejects conflicting identity reuse, then restores into another fresh
native backend. It checks exact output rows, revision, empty relation inventory,
exclusion of the unpublished mutation and a post-restore retraction.

```
cargo +1.95.0 build --locked --features iceberg --example iceberg_recovery
# Configure the ordinary pinned DDlog native environment first.
target/debug/examples/iceberg_recovery stage /tmp/fresh-run /absolute/build-ddlog.sh file:///tmp/fresh-run/warehouse
target/debug/examples/iceberg_recovery publish /tmp/fresh-run /absolute/build-ddlog.sh file:///tmp/fresh-run/warehouse
target/debug/examples/iceberg_recovery restore /tmp/fresh-run /absolute/build-ddlog.sh file:///tmp/fresh-run/warehouse
```

For bounded real R2 acceptance, the example accepts
`r2://archetype-staging/ddlog-acceptance/UNIQUE_RUN`. Its example-only FileIO
adapter uses Cloudflare's authenticated REST object API and environment variables
`CLOUDFLARE_ACCOUNT_ID`/`CLOUDFLARE_API_TOKEN`; credentials never enter table
metadata or receipts. Use existing authorized credentials. The adapter limits
objects to 2 MiB, disables broad deletion, fetches actual remote bytes for every
read, and retains no local object cache. It deliberately buffers small objects;
it is not an S3 throughput benchmark or recommended production driver. Production
callers supply their catalog and FileIO implementation (including native S3).

The automated contract test uses a clearly simulated DDlog transport with real
local Iceberg/Parquet. Native and R2 acceptance must be reported separately.

## Recorded verification and provenance

[Source-bound acceptance](evidence/iceberg-runtime.json) records successful native
local and real R2 runs. Each uses separate stage/publication/restore processes and
a new native backend, the current runtime build driver and existing pinned native
toolchain/cache. Both restore the exact state and process a subsequent retraction.
The example's remote adapter is REST FileIO, not native S3 or a throughput result.
The SQL catalog remains local; data files and Iceberg table metadata are remote.

This promotes the mechanism demonstrated in the September 5 controlled
[DDlog recovery prototype](https://github.com/VangelisTech/archetype/blob/f324af21/spikes/ddlog-recovery/RESULTS.md)
and [deferred publication prototype](https://github.com/VangelisTech/archetype/blob/f324af21/spikes/iceberg-v3-roundtrip/RESULTS.md)
into the owning runtime. Those historical fixtures used an external input ledger
and retained binaries; this implementation captures and restores runtime-owned
state and invokes the current native build driver. Their older isolated dependency
locks are not imported here. X0 and Holocron deployment pins are not modified.
