# Logical program and world creation

These trusted Rust ports extend the existing `ProcessorRegistry` and
`WorldManager`. The registry owns exact program identity; the manager owns world
reservation, materialization and activation. A storage host owns published
context visibility and must verify its exact descriptor before acknowledging
the context to the manager. There is no additional execution owner or scheduler.
The ports do not add stdio or MCP operations. Existing raw `register` and `create`
calls still allocate identities on each call.

## Registry publication

`publish_logical_program(LogicalProgramRequest)` binds a logical resource and
request key to the full immutable request, including description, definition,
lowering version and Git provenance. The existing registry update lock covers
preparation, immutable version publication, initial current pointer and logical
publication acknowledgment. Preparation retains the allocated processor ID,
exact version record and resolved composition before publishing them.

`resolve_logical_program(resource)` requires neither a request key nor a current
pointer lookup. It returns the original exact processor reference with phase
`prepared` or `published`. Published resolution verifies the retained preparation
and exact version. It does not compile, activate or move a pointer. A retry of
the identical request reconciles that same pin; changed contents, another key at
the same resource, or the same key at another resource conflict. Advancing or
archiving the processor's current version cannot redirect or reactivate the
logical pin, including when publication was interrupted before acknowledgment.

The initial canonical processor version retains the logical resource, request
key and request digest as optional version metadata. Before allocating a new
logical program, the registry streams retained canonical versions, including
versions with no current pointer. Missing or mismatched preparation beneath a
retained association fails closed. Losing preparation and acknowledgment after
version publication therefore cannot allocate a second pin, in the same process
or after reopen. Existing version bytes without this optional metadata keep
their meaning and serialization. Exact imports preserve the association; importing
such a version alone does not reconstruct its logical preparation, and logical
creation remains blocked until that retained authority is reconciled.

This is detection from retained authority, not recovery after arbitrary erasure.
If preparation is lost before any canonical version exists, or every associated
authority record is erased, the registry has no published fact from which to
recover that name. It does not claim otherwise or automatically repair it.

New publication validates active exact composition dependencies. Reconciliation
uses retained exact dependencies and does not require them to remain active.
Existing registry stale-lock rules still apply after an abrupt process death;
callers must reconcile lock ownership rather than delete the lock and blindly
repeat raw registration. An uncertain fsync outcome preserves evidence and may
require retrying the same logical request.

## World reservation and context readiness

`reserve_creation(CreationRequest)` uses a closed `LogicalDestination` with
`resource`, `world` and `run`. Identity is separate from the immutable world
definition, opaque host binding declarations and optional exact fork source.
The same key and complete payload return the same native world ID. Changing
payload conflicts. Another key conflicts if either the resource matches or the
world/run pair matches, preventing aliases from allocating a second world.

Fresh and fork requests share the existing `forks` reservation catalog and one
materializer. V1 fork JSON, hashes, source proof and materialization markers keep
their meaning. V2 logical records wrap fresh or fork origin explicitly. A V1
destination of exactly `{world, run}` with valid tokens participates in shared
scope occupancy. Other legacy destinations require an explicit operator mapping
before mixed logical creation; the manager does not invent one.

Reservation is durable before the world control record. Reconciliation re-fences
the retained reservation, world file, child directory and owner directory before
accepting the materialization marker. Losing a materialized world fails closed
instead of reconstructing generation zero. Missing reservation evidence beneath
a loaded world, or orphan materialization/context evidence, also fails closed.
Catalog validation streams bounded payloads and retains only compact identities;
recovery materializes them in a second pass after the whole catalog is validated.

`resolve_creation(destination)` finds the reservation without the old request
key, reconciles unfinished materialization and returns retained definition,
binding, context phase and any fork reservation. It never starts a compiler or
replays input. A fresh world remains `created` at generation zero.
`lookup_creation(destination)` supplies the same resolution with typed `None`
for an absent name, permitting host preflight without interpreting error strings.
Corrupt or incomplete authority is an error, never absence.

`confirm_creation_context(reservation, context_id)` is a trusted host operation
after exact storage-owned descriptor readback. It first persists an immutable
context candidate in the same reservation catalog, then persists world readiness.
A failed world write leaves the exact candidate recoverable after reopen and
blocks activation until that same candidate is confirmed. Missing or changed
candidate evidence beneath a ready world fails validation. Confirmation itself
neither starts execution nor seeds an external publication head.

Logical worlds reject activation and bound restore while the context is pending.
Forks additionally require the existing exact source restore and lineage
acknowledgment; context readiness does not bypass that gate. Admission and
external-publication rules remain those in [external publication](external-publication.md).

## Bounds and evidence

Logical tokens are 1–128 ASCII letters, digits, `-`, `_`, `.`, or `:`. Each owner
catalog admits at most 1,024 logical programs or world reservations. Registry
requests are at most 1 MiB, descriptions 4,096 bytes, and the actual pretty-encoded
preparation at most 2 MiB. World reservation JSON is at most 2 MiB; host binding
JSON is at most 64 KiB and depth 32. Context candidates are bounded to 16 KiB and
carry a 64-character hexadecimal context ID. Existing exact definition,
composition, checkpoint and external-output bounds also apply.
New logical program allocation also caps canonical authority inspection at
16,384 processor directories and 16,384 version files, reading one record of at
most 2 MiB at a time. A registry exceeding these bounds rejects this operation
before allocating a new identity; existing raw registry operations are unchanged.

`tests/processor_registry.rs` covers every publication interruption, payload and
destination conflicts, retained pins after pointer/lifecycle changes and encoded
size admission. `tests/worlds/boundary.rs` covers reservation/materialization
faults, context failure across reopen, missing evidence, shared legacy occupancy,
fresh generation-zero behavior and separate fork gates. These tests use controlled
native transport. They are not actual-DDlog compiler acceptance; the separately
configured [native acceptance](building.md#native-acceptance) retains that scope.
