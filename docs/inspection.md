# Inspection metadata version 1

`inspection::InspectionMetadata` is a serializable, validated authored contract.
Call `validate` before registering a definition and `validate_with_graph` when
binding it to a live native graph. Unsupported schema versions fail validation;
a reader must not silently interpret them as version 1.

An authored group has a stable `id` and `member_key` (the composition member's
qualified identity). Its repository/revision and optional one-based source range
identify authored provenance. A revision is caller-supplied provenance, not proof
that a remote repository or release exists. This module does not query Git.

`authoredGroups` and `memberIds` match the observer's existing grouping field names.
`memberIds` are exact native IDs, including worker distinctions. Groups are separate
from `NativeGraph.nodes`; they never become operators. Native operators retain
`id`, `worker`, `address`, `name`, and `debug`. Channels retain `id`, `worker`,
`scope`, `source`, `target`, `source_port`, and `target_port`. Validation is read-only.
Native IDs and addresses must never be reconstructed from display labels.

Ports carry a stable authored ID, name, direction, and source-language `data_type`.
A port can map to several native operator/index pairs. Mapping indexes are native
port indexes, not layout port IDs. Live validation requires a matching channel in
the stated direction. Unconnected ports cannot be verified from channel data and
must have no native mapping until supporting observations exist.

Version 1 groups without a `kind` are flat and disjoint boxes: overlapping native
membership is rejected. `kind: "module"` groups are blocks (below): they may nest
and a block's members repeat those of its nested blocks.
Registered definitions may have no native member IDs or mappings; registration is
not execution. Live bindings are execution-specific and must be revalidated after
recompilation or restart. Clients still decide whether a group can be laid out
within native scopes; visual geometry is deliberately not part of this contract.
No memory usage, CPU usage, liveness, or process ownership is inferred from a graph.

## Membership by match

Native operator ids are not stable across builds, so a group may declare
`member_match` instead of (or in addition to) `memberIds`, and a port may declare
`native_match` instead of `native_ports`. Both are omitted from the wire form when
absent, so definitions without them keep their content hash.

- `member_match: {scope_name?, debug_pattern?}` — at least one. `scope_name` names a
  native scope (an operator with nested children, e.g. a `region_named` phase) and
  claims every matching scope with its whole subtree. `debug_pattern` is a regular
  expression searched in the operator `debug` text (the DDlog profiler context).
- `native_match: {debug_pattern, index}` — the port binds to native port `index` of
  the one group member whose `debug` matches.

`validate()` (registration time) checks shape only for match-based groups: the
pattern compiles, `scope_name` is non-empty, ids are unique. Membership is resolved
by `resolve(graph)` when a snapshot is taken, deterministically:

1. Groups are processed in declaration order. Explicit `memberIds` and every
   `scope_name` match claim first; a claim on an operator already owned by another
   group is a mapping error.
2. Then `debug_pattern` matches, again in declaration order, claim only operators
   nobody claimed yet.
3. Zero matches for a group, a `native_match` that resolves to zero or several
   members, a port mapped outside its group, a member whose native children are not
   all members (a split scope), or top-level members under different native parents
   are mapping errors naming the group.
4. The resolved metadata then passes the existing live checks (ids exist, ports are
   observed on channels in the stated direction).

A snapshot reports the resolved metadata (`authoredGroups[*].memberIds` are the
resolved ids, `ports[*].native_ports` include the matched binding, plus a
`mapping_report`) and `mapping_error: null`, or the authored metadata unchanged
with the error text. Resolution is a pure function of the metadata and the observed
graph; it never consults the registry or invents operators.

## Module groups (blocks) and neighbour propagation

An `AuthoredGroup` may carry `kind: "module"` and its `member_match` may carry
`propagate: true`; both are omitted from the wire form when absent, so existing
definitions keep their content hash. A module group is a **block, not a box**:

- It skips the layout invariants (whole native subtree, one native parent) and
  the zero-match rule: a block whose pattern matches nothing resolves to an empty
  `memberIds` (its report shows `matched: 0`) instead of a mapping error, because
  the runtime synthesizes blocks without knowing which generated relations the
  DDlog compiler keeps as named operators. Viewers never draw blocks as boxes.
- Blocks nest by id: `<parent>/<child>` is a nested block of `<parent>` when both
  have `kind: module`; a parent is declared before its children and its resolved
  `memberIds` include every nested block's members. Other groups stay disjoint.
- `validate()` requires a module group to declare `member_match` or `memberIds`.

**Propagation.** After steps 1–2 above, every unclaimed operator adopts the group
with the most channel neighbours (source or target of any observed channel, self
loops ignored) among groups whose `member_match.propagate` is `true`; groups
without the flag never vote, so a box never grows through propagation. Ties go to
the lowest group index (declaration order). Rounds are synchronous — every adoption
of a round is computed from the previous round's owners — and repeat until nothing
changes or 16 rounds have run (`PROPAGATION_ROUNDS`); an operator whose neighbours
are still unowned after that stays outside every group. Propagation happens before
the nested-block aggregation, so an operator adopts the deepest block that reached
it and its ancestors list it too.

Resolved metadata carries the report:

```json
"mapping_report": {"groups": {"<group id>": {"matched": 3, "propagated": 5}},
                   "unattributed": 2}
```

`matched` counts operators claimed by `memberIds`, `scope_name` or `debug_pattern`,
`propagated` those adopted through neighbours, both for the group that owns the
operator directly (a nested block's members are not re-counted on its parent), so the
sum over groups plus `unattributed` is the number of native operators. Every group
is listed, including boxes (`propagated: 0`). The retained capture stores the
resolved blocks and the report like any other resolved metadata.

Worlds built from a composition receive synthesized module groups from the world
manager (`worlds.md`): one per composition node matching the generated relation
prefix (`\bR_Module<index>_`, `\bR_Composite<index>_` for a nested composition,
whose own nodes become `<alias>/<child>`), plus `$inputs` (`\bR_Input_`) and
`$outputs` (`\bR_Output_`), all with `propagate: true`. Hand-authored groups keep
today's semantics and precedence.

## Native capture build hook

The standard build driver installs `native/observer.rs` into generated DDlog Rust.
`scripts/install-observer.py` accepts the generated project directory and verifies
both pinned generator patch sites before writing. Repeated installation is a no-op;
changed/duplicate sites fail rather than compiling without observation support.
Python 3 is required by this build step. Runtime logging and debug regions remain
opt-in through `DDLOG_OBSERVER_FILE`; building alone does not capture anything.

Capture records are actual Timely, progress, and differential events. The writer
limits each worker to a byte budget (`DDLOG_OBSERVER_BYTES`, default 64 MiB; values
below 4096 or non-integers fail the install). Without rotation it emits a
`capture_status` record with `status: truncated` and `reason: worker_byte_limit`
when its budget is exhausted and writes nothing further. With
`DDLOG_OBSERVER_ROTATE=1` it instead truncates the shared file to zero, writes
`{"stream":"capture_status","status":"rotated","bytes":N}` as the first line of the
fresh file and continues; if truncation fails it falls back to the truncated marker
(`reason: rotation_failed`). Consumers must report incomplete capture when a
truncation marker appears; absence of additional events is not proof of idle
execution. Rotation discards earlier bytes of every worker (the budget is per
worker, the file is shared), so a reader must have ingested topology before the
first rotation; the runtime's reader keeps what it ingested and restarts at offset
zero when the file shrinks. The runtime must select a private capture path. Raw
`operator_id` and `channel_id` are retained independently of composite display
IDs, together with source and target addresses and native port indices.

Library capture defaults to topology-only (actual operator and channel creation).
`Backend::set_observer` (`ObserverOptions {detail_full, rotate}`) sets
`DDLOG_OBSERVER_DETAIL=full` and `DDLOG_OBSERVER_ROTATE=1` for the native child;
managed worlds turn both on, library users leave both off. The runtime only adds the
variables it is configured with and never strips inherited `DDLOG_OBSERVER_*`
variables, so an operator who exports them to a library host or the MCP server still
gets the capture they asked for. Full tracing adds runtime overhead and is not
required by the world inventory.

## Reader, state and activity

`telemetry::Reader` owns a capture file position (path, offset, pending partial
line) and is driven by a tailer thread or a one-shot pass; `telemetry::State` is
the shared ingested view (`Arc<Mutex<State>>`) that `status` snapshots. Each tick
reads at most 4 MiB; a single event line above 1 MiB, a malformed record or a
conflicting operator/channel identity is a sticky error. Ingest handles `Operates`,
`Channels`, `Schedule` (Start/Stop pairs → `schedule_count`, `busy_ns`,
`last_seen_ns`), `Messages` (`is_send` → channel `message_count`, `records +=
length`), `Shutdown` (`active: false`), `stream: progress` (counter),
`stream: differential` (per-operator `arrangement_events`, `last_arrangement_event
{kind, time_ns, length}`) and `capture_status` (`truncated`, or `rotated`, counted
once per rotation whether the shrink or the record is observed first).

`inspection.state` is `missing` (no capture file yet), `failed` (sticky error),
`truncated`, `pending` (no operators yet) or `available` (no error, not truncated,
operators observed). Lag never changes the state; it is reported in
`inspection.activity`:

```json
"activity": {
  "nodes":    {"<node id>": {"schedule_count":0,"busy_ns":0,"last_seen_ns":0,"active":true,
                             "arrangement_events":0,"last_arrangement_event":null}},
  "channels": {"<channel id>": {"message_count":0,"records":0}},
  "totals":   {"events":0,"timely":0,"progress":0,"differential":0,
               "operators":0,"channels":0,"unresolved_channels":0},
  "last_event_ns": null, "progress_events": 0,
  "complete": true, "truncated_at_bytes": null, "rotations": 0,
  "lag_bytes": 0, "last_ingest_unix_ms": 0
}
```

`complete` is whether the reader has ingested every complete line the file held at
its last tick without error or truncation. Every observed operator and channel has
an activity entry (zeros until events arrive); events naming an operator that never
reported `Operates` are kept under that id as well. Channels whose endpoints are not
known are counted in `unresolved_channels` and never emitted as edges.
