# Managed worlds control plane

`ddlog-worlds REGISTRY BUILD_ROOT BUILD_DRIVER` owns a single `WorldManager`.
All three paths must be absolute. Registry and build roots must be dedicated,
owner-private directories. An exclusive advisory lock rejects another manager for
the same build root. Clients attach to the existing owner; constructing another
manager is not discovery. Different stores can deliberately own different worlds.

The stdio protocol is newline-delimited JSON, capped at 1 MiB per request.
Requests have `operation` and `args`; replies have `ok` and either `result` or
`error`. There is no network listener, arbitrary host-process discovery, or daemon
autostart. The operator chooses when to run this owner.

- `runtime_info` `{}` returns `{commit, dirty, crate_version, schema_version}` recorded by
  `build.rs` at compile time. `commit` is null when the crate was not built from a git
  checkout; `dirty` is whether tracked files differed from that commit.
- `library_create` registers `{name, repository, revision}`; `libraries` lists every
  entry with its `processors` (`[{processor_id, version, name, description}]`) plus the
  implicit `unassigned` library (name "Unassigned", always last). A (processor, version)
  belongs to exactly one library; a later association moves it. Libraries are
  operator-supplied provenance, not proof of executable DDLog definitions.
- `register` takes `name` (required), optional `description`, optional `library_id`
  (default `unassigned`), `definition` and optional `git_provenance`. The reply is the
  registry record plus `name`, `description` and `library_id`. Every registered
  definition has a name; the UI never has to show a processor id.
- `definitions` `{}` returns `{"processors":[…]}` with one row per **version** of every
  processor (archived included), looping registry pages internally:
  `{processor_id, version, current, kind, status, name, description, library_id,
  created_at_unix_ms}`. `kind` is `program` or `composition` (also fixed in the
  registry's own `ProcessorSummary`). A version without an association is listed under
  `unassigned` with a derived name: `outputs ← inputs` from its public interface, or
  `Composition of <aliases>`. `definition` reads an exact `processor_id` and `version`.
- `import` `{source_registry, processor_id?, library_id?, names?: {"<pid>": "…"}, dry_run?}`
  copies version records from another registry directory **preserving `processor_id`
  and `version`**. The source is read with plain JSON reads of `<pid>/current.json` and
  `<pid>/versions/*.json`; it is never locked, permission-checked or written, so a 0755
  fixture registry is a valid source. Every version of every selected processor is
  imported, dependency closure first; a processor new to this registry gets
  `current.json` copied from the source, an existing processor keeps its pointer. Each
  record is envelope-, hash- and definition-validated (compositions resolve against
  source-plus-destination records) **before anything is written**; any error returns
  `errors: [{processor_id, version, error}]` with nothing imported. The reply lists
  `imported: [{processor_id, version, kind, status: imported|present, current, name,
  library_id}]`. An identical existing version is `present`; a same-name record with
  different content is an error. `dry_run` validates and plans without writing.
  Imported definitions are named from `names`, an existing association, or derived.
- `create` takes a `label`, exact `processor` reference, optional `purpose`
  (`instance`, the default, or `test`) and, for tests, non-empty `scenarios`. It does
  not start a process. Scenarios are validated at creation.
- `start` takes `id`, records `starting`, then compiles/installs asynchronously.
  Poll `status`, `inspect`, or `inventory` to collect completion or failure.
- `stop` takes `id`. It cancels managed compiler/native process groups. A pending
  install reports `stopping` until its completion is collected, then `stopped`.
- `inventory` `{processor_id?, version?, summary?}` lists worlds, optionally filtered by
  pin. `summary: true` returns each status **without** `inspection`, `instance` and
  `managed_processes` and never calls the live instance; it is what a sidebar polls.
- `execute` takes world `id`, an inner `operation`, and inner `args`. Definition
  installation/registry mutations are rejected here; world definitions stay pinned.
  Beyond the existing program operations it offers:
  - `instance_info` adds `revision`, `program_version` and `source_sha256` (sha256 of
    the lowered `program.dl`) while the instance is healthy.
  - `apply_changes` adds `revision`.
  - `relations` `{}` → `{revision, relations: [{name, input, fields, count}]}` over the
    program's public relations. Output counts are a streamed native scan per relation
    (rows are never accumulated); input counts come from retained facts.
  - `query_rows` `{predicate, max_rows (1..=10000, default 500), continuation?}` →
    `{predicate, revision, fields, rows, total, complete, continuation}`. Outputs page
    through the bounded native reader, inputs page over retained facts; `total` is the
    full count at `revision`; rows are typed by `fields` (int → number, string →
    string). Continuations are opaque and invalid after any mutation.
  - `program_source` `{}` → `{source, source_sha256}`.
- `scenarios_set` `{processor_id, version, scenarios}` / `scenarios_get` store and read
  `<BUILD_ROOT>/scenarios/<processor_id>/<version-hex>.json`. A scenario is
  `{name, description?, changes: [{op: insert|delete, predicate, values}], expect:
  {"<relation>": [[…], …]}}`. Names are unique and non-empty; change predicates must be
  public inputs, expected relations public relations; arity and types must match the
  declared fields; expected rows must have no duplicates (relations are sets).
- `test` `{processor_id, version, scenarios?: [names], keep_world?}` creates a world
  `{label: "Test · <name>", purpose: test, scenarios}` from the stored scenarios,
  starts it and returns its `starting` status immediately. After a successful install
  the start thread applies each scenario's changes cumulatively in order, reads every
  expected relation through `query_rows`, compares as sets and records the results;
  it then drops the instance (the world becomes `stopped`) unless `keep_world`.
- `capture_get` `{processor_id, version}` returns the retained compiled topology of
  that definition version (below) or `{state: "missing"}`; an unknown pin is an
  error.

**Public relations rule** (used by `relations`, `query_rows` and scenario validation): a
program without an interface exposes every declared schema; a program with an interface
exposes its interface inputs and outputs; a composition exposes its resolution inputs and
outputs. The runtime exposes this as `ddlog_runtime::instance::public_relations(record)`.

A world progresses from `created` to `starting` to `running`, or to `failed`.
Stopping a running world drops and reaps its native child. Restart is explicit and
uses a fresh generation/build directory. When `status` observes that the native
child died it reports `failed` and reaps the instance, so `start` is valid for
`failed` worlds (crashed or build-failed) and opens the next generation.
The independent shutdown handle cancels all tracked process groups and prevents
further starts for that manager. Ordinary owner drop cancels execution; SIGINT and
SIGTERM cancel process groups before the executable exits. SIGKILL and machine
failure cannot promise synchronous cleanup.

`status` carries, besides definition, state, generation, error, history, owner,
resources, managed processes, instance info and inspection:

- `revision`: the live instance's committed revision (an in-memory counter, present in
  summary inventories too), or null when no instance is live.
- `started_at_unix_ms`: the `at_unix_ms` of the latest `starting` history entry of the
  current generation, or null.
- `build`: `{generation, log_tail, instrumented}` while `starting` or after a failed
  build, otherwise null. `log_tail` is the last 60 lines of
  `<BUILD_ROOT>/<id>/<generation>/build-1/build.log` and null until the driver creates
  that file. `instrumented` is whether the bundled build driver's `install-observer.py`
  completed for that build (it records `program_ddlog/observer-install.json`): the
  native observer hook is present and, when the program imports the built-in star
  library, its generated copy carries the `large-star`, `small-star` and
  `minimum-label` phase regions. That patch is applied to the generated project at build
  time; `src/star/lemmalog_star.rs` and the registry content hashes are unchanged by it.
- `persistence`: `{status: "not_configured", reason: "world checkpoints are not wired"}`
  in this slice; nothing claims durable world state.
- `test`: for test worlds, `{phase: building|applying|done|failed, scenario_index,
  results: [{name, passed, revision, expected, observed, missing, unexpected, error}],
  passed: bool|null, error}`, persisted in `world.json`. `expected`, `observed`,
  `missing` and `unexpected` are keyed by relation name. A build failure reports
  `phase: failed` with the build error; a scenario whose changes or reads fail carries
  that error in its result and fails.

Definitions, generation, state, errors, metadata and transition history are written
into each world's record. History is retained across owner restart, but running
processes are never adopted. Previously active records become `interrupted` with an
explicit error. Historical native event files can still be inspected; their presence
is not evidence that a world is currently running. Failed durable writes surface as
errors and do not suppress subsequent persistence retries.

Resource samples report a timestamp, PID, CPU percentage and resident bytes, with
missing/unsupported states and a suggested maximum age. CPU comes from `ps`: a
lifetime average on Linux and a decaying average on macOS, not an interval delta.
The primary sample covers only the native child. Managed-process entries also show
tracked compiler/startup process leaders; descendants are excluded. The embedding
owner's memory is shared and not attributed to individual worlds. These are not
operator-level memory measurements. Samples are synchronous observations and can
race process exit; neither measurements nor cached graphs prove continuing liveness.

Native graph inspection is optional build-driver support. The bundled driver
installs the opt-in hook described in `inspection.md`; worlds start the native child
with `DDLOG_OBSERVER_DETAIL=full` and `DDLOG_OBSERVER_ROTATE=1` so schedule, message,
progress and arrangement events are captured and a long-lived world rotates its
capture at the worker byte budget instead of going silent. Capture can be missing,
pending, truncated, failed, or available; lag is reported in `inspection.activity`
(`lag_bytes`, `complete`, `rotations`) and never makes a world unavailable. Mapping
errors remain explicit. Authored metadata binds native operators either explicitly or
through `member_match`/`native_match` resolved at snapshot time (`inspection.md`);
source/debug labels are never converted into fabricated membership. Consumers must
show lifecycle state alongside captured topology.

**Tailer.** Each `start` spawns one capture tailer thread for the new generation. It
polls the capture file every 100 ms, ingests at most 4 MiB per poll into the shared
state that `status`/`inspect` snapshot, and exits on `stop`, on owner drop (both
join it after a final poll), when `status` observes the native child dead, or after
50 unchanged polls once the world has no live instance and no pending start (a test
world that dropped its instance, a failed install). Recovered `stopped`/`interrupted`
worlds read their last generation's capture once, on their first full `status`, and
hold that state without a thread; `inventory {summary: true}` never reads a capture.

**Retained topology.** Once per generation, on the first full `status` where
`inspection.state == "available"`, `unresolved_channels == 0` and at least one
`Schedule` event has been ingested, the runtime writes
`<BUILD_ROOT>/captures/<processor_id>/<version-hex>.json`:
`{schema_version: 1, processor: {processor_id, version}, world_id, generation,
captured_at_unix_ms, unresolved_channels, graph: {nodes, edges}, metadata,
mapping_error}` — topology and resolved authored metadata only, no activity. A later
generation of any world pinned to that version replaces it. `world.json` records
`capture_generation` so a recovered world does not capture the same generation twice.

This slice supplies the runtime contract and lifecycle owner. A later library
extraction/integration slice can migrate independent applications onto registration
and their own supported APIs without merging their repositories or claiming those
services are already managed. X0 and Holocron provenance registration alone does
not instantiate their programs or imply integration coverage.
