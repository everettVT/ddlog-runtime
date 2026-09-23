# BackendRuntime: TLA+ model of the program runtime

`BackendRuntime.tla` models one or more `Backend`s (`src/lib.rs`), each wrapped in a
`ProgramInstance` admission guard (`src/instance.rs`), together with bounded reads
(`src/bounded.rs`) and explicit checkpoints (`src/checkpoint.rs`). Each action carries a
comment citing the lines it models.

Run from this directory:

```
java -XX:+UseParallelGC -cp /tmp/claude-0/tla2tools.jar tlc2.TLC -workers auto \
     -config BackendRuntime<X>.cfg BackendRuntime.tla
```

Use `-workers 1` on the bug configs to get the shortest counterexample. TLC version:
2026.09.22.222048 (rev 35d40c9, the jar at `/tmp/claude-0/tla2tools.jar`), Java 21.

## What is modelled

| Model variable | Code | Notes |
|---|---|---|
| `facts[b]` | `Backend.facts` (lib.rs:207) | Retained, acknowledged inputs |
| `native[b]` | not a code variable | Input set the live child has actually committed. Outputs are a function of it. |
| `runtime[b]` | `Backend.runtime` (lib.rs:205) | 0 = `None`, otherwise the child's `Runtime.identity` (lib.rs:94, bounded.rs:18-28) |
| `alive[b]` | child process | Can die at any time (`ChildCrash`) |
| `failed[b]` | `Backend.failed` (lib.rs:212) | |
| `prog`, `version`, `revision` | lib.rs:208-211 | |
| `ownerCtr` | `OWNER_SEQUENCE` (bounded.rs:16) | Global, so identities are unique across backends |
| `qcur[b]`, `icur[b]` | `QueryCursor` (bounded.rs:59-67), `InputCursor` (instance.rs:83-88) | Latest cursor issued by `b`. A caller may present it later, after any number of steps. |
| `ckpt` | latest checkpoint (checkpoint.rs:17-35) | Ghost field `consistent` records `facts = native` at save time |
| `epoch[b]` | ghost | Counts state changes on `b` (ack, install, restore): the ground truth for cursor soundness |
| `everFailed`, `badAccept`, `ackWhileFailed`, `readPoisonLive` | ghost | History flags used by the properties |

**Actions:**
- **Install**, `install_source` (lib.rs:351-429). Outcomes: compile fail, start fail, replay reject or I/O fail (the candidate identity is consumed and the prior program is untouched), or ok (replay retained facts into a new child, set `failed := FALSE`, bump version and revision).
- **Apply**, `apply_inner` (lib.rs:460-522). `dump` = `apply` / MCP `apply_changes`; `¬dump` = `apply_without_deltas`. Outcomes:
  - validation failure: no effect
  - committed and acked
  - native rejects with error text: acked when `dump` (bug), poisons otherwise
  - I/O error or child death before the native commit
  - I/O error or child death after the native commit
  - reply over 4 MiB (deltas only, after the commit)
- **Reads.**
  - `Query`: non-streaming `read_runtime` (lib.rs:523-549), which can overflow 4 MiB and poison.
  - `BoundedRead`: streaming, issues a `QueryCursor` (bounded.rs:97-213).
  - `InputRead`: issues an `InputCursor` (instance.rs:357-389).
  - `UseQCursor` / `UseICursor`: the exact checks at bounded.rs:124-135 and instance.rs:362-369.
- **`ObservedHealth`** (lib.rs:270-278): `try_wait` reports exit, or returns `Err` when `TryWaitErr` holds. Sets `failed` but keeps `runtime`.
- **`SaveCheckpoint`** (checkpoint.rs:169-239): pure program, health ready. **`RestoreOk` / `RestoreFail`** (checkpoint.rs:262-285): fresh backend only.
- **Mode `Hosted`:** the guard at instance.rs:152-158. TRUE = host or worlds (instance_id is Some); FALSE = standalone, or direct library use.

**Environment switches** (TRUE = adversarial behaviour possible): `NativeErrorText`, `LargeOutputs`, `TryWaitErr`, `CrossInstanceCursors`.

**Fix switches** (FALSE = code as written): `FixDeltaCheck`, `FixReadDrain`, `FixInputOwner`, `FixFailedGuard`.

### Abstractions and omissions
- **Facts and bounds:** Facts = {f1}. A transaction is abstracted to its staged result set S ⊆ Facts (lib.rs:465-479), and the net diff sent to the child is implied. Bounds: MaxRev = 2, MaxVer = 2, MaxOwner = 3 (global across 2 backends), Programs = {p1, p2} with p2 impure (`agent_*`). Exhausting a bound disables the action, which matches the `checked_add` refusals at lib.rs:369-373 and 464.
- **Cursor fields:** predicate and filter binding is not modelled; it is a plain equality check with no state interaction. Cursors carry `from`, `owner`, `rev` and `epoch`.
- **Atomic steps:** each action is atomic. host.rs:330-340 holds the owner mutex for the whole `handle_line`, so operations from different connections never interleave. Lost replies after commit (host.rs:342-344) are not modelled.
- **Not modelled:**
  - the schema-compatibility guard (lib.rs:357-368), treated as part of "compile fail"
  - registry pins, interfaces, agent-operation bookkeeping
  - checkpoint file I/O and digests
  - restore discarding `control`/`observer` (report issue 6)
  - build-dir leaks

## Properties

| Property | Kind | Meaning (from docs/specification.md) |
|---|---|---|
| `RetainedMatchesNative` | invariant | "acknowledged state changes only after completion": health ready ⇒ `facts = native` |
| `CursorSound` | invariant | A cursor is accepted only if it comes from the same backend and live owner, with no mutation, reinstall or restore since it was issued |
| `NoAckWhileFailed` | invariant | No input transaction is acknowledged by a backend that reports `failed` |
| `HostedPoisonSticky` | invariant | Hosted: once failed, always failed ("uncertain native failure disables continued use") |
| `ReadsPreserveHealth` | invariant | A read of a live child never disables the owner |
| `CheckpointSound` | invariant | A checkpoint comes only from a ready, pure backend, holds native-committed inputs, and has `rev ≥ ver ≥ 1` |
| `VersionLeRevision` | invariant | `version ≤ revision` |
| `TypeOK` | invariant | Types |
| `RevisionMonotone` | action | The revision never decreases |
| `RevisionOnlyOnAck` | action | The revision changes only on a real native commit (+1), a successful install (+1), or a restore |
| `FailedReplacementPreserves` | action | Compile, start or replay failure leaves program, facts, runtime, revision and version unchanged |
| `RestoreReproduces` | action | Restore yields checkpoint facts = native, revision, version and program, with health ready |

## TLC results

The configs are 2 backends, Facts = {f1}, MaxRev = 2, MaxVer = 2, MaxOwner = 3, all 12 properties, and `CHECK_DEADLOCK FALSE` (bounds cause intended deadlock).

| Config | Setting | Result |
|---|---|---|
| `BackendRuntime.cfg` | intended environment, **hosted**, code as written | **No error.** 7,193,589 states generated, 885,928 distinct, depth 15, 42 s |
| `BackendRuntimeStandalone.cfg` | intended environment, standalone | **No error.** 10,740,165 generated, 1,001,352 distinct, depth 15, 1 min 26 s |
| `BackendRuntimeFixed.cfg` | all adversarial switches on, all fixes on, standalone | **No error.** 19,065,863 generated, 1,514,716 distinct, depth 15, 2 min 26 s |
| `BackendRuntimeFixedHosted.cfg` | same, hosted | **No error.** 15,390,043 generated, 1,371,500 distinct, depth 15, 2 min 04 s |
| `BackendRuntimeBug1.cfg` | `NativeErrorText` | **RetainedMatchesNative violated** (depth 3) |
| `BackendRuntimeBug1Ckpt.cfg` | same, checks `CheckpointSound` only | **CheckpointSound violated** (depth 4) |
| `BackendRuntimeBug1Rev.cfg` | same, checks `RevisionOnlyOnAck` only | **RevisionOnlyOnAck violated** (depth 3) |
| `BackendRuntimeBug2.cfg` | `LargeOutputs` | **ReadsPreserveHealth violated** (depth 3) |
| `BackendRuntimeBug3.cfg` | `CrossInstanceCursors` | **CursorSound violated** (depth 5) |
| `BackendRuntimeBug4.cfg` | `TryWaitErr`, standalone | **NoAckWhileFailed violated** (depth 4) |
| `BackendRuntimeReach.cfg` | sanity witness | `NeverRestored` violated as expected: Install, Install, SaveCheckpoint(b1), RestoreOk(b2). Restore is reachable. |

"Intended environment" means:
- the native child never prints error text on `commit dump_changes`
- no reply exceeds 4 MiB
- `try_wait` never returns `Err` for a live child
- callers present a cursor only to the instance that issued it

In that environment the code as written satisfies every property, in both modes. With every adversarial behaviour enabled, the four proposed fixes together restore every property.

## Counterexamples, mapped to code

### Issue 1: `apply` accepts native error text as a commit
Trace: `InstallOk(b1,p1)`, then `ApplyErrorText(b1,{f1},dump=TRUE)`.

1. The child rejects the transaction and prints an error, so it does not commit and `native = {}`.
2. lib.rs:497-503 only treats non-empty output as an error when `!dump_deltas`, so the text is returned as `deltas`.
3. lib.rs:505-507 then sets `facts = {f1}` and `revision = 2`.
4. Result: health is ready but `facts ≠ native`. The same run also violates `RevisionOnlyOnAck` (revision advanced with no native commit). One more step, `SaveCheckpoint(b1)`, persists inputs the program never acknowledged (`CheckpointSound`).

**Fix:** under `commit dump_changes`, accept only well-formed delta lines. Each line must be a relation header `R_x:` or a row `R_x{...}: ±n`. Anything else takes the existing error branch (poison, retained facts not advanced), matching the plain-commit and replay paths (lib.rs:498-499, 413-417).

### Issue 2: a read poisons a healthy instance
Trace: `InstallOk(b1,p1)`, then `Query(b1)` with a dump over 4 MiB.

1. `exchange` returns an error without draining (lib.rs:117-118).
2. `read_runtime` sets `runtime = None; failed = true` (lib.rs:531-537).
3. A pure read has disabled a live owner. Hosted, the instance is then permanently refused (instance.rs:152-158). The same holds for `why` and `query_typed`.

**Fix:** implement `query`, `query_typed` and `why` on `exchange_stream` (lib.rs:130-178), or make `exchange` drain to the marker before returning the size error. Then an oversized reply is a local error, as for bounded reads (bounded.rs:195-197), and only I/O or framing errors poison. Oversized *deltas* after a commit (`ApplyIOAfter`, "ApplyOversizedDeltas") correctly stay a poisoning, uncertain outcome.

### Issue 3: `InputCursor` accepted by another instance
Trace: `InstallOk(b1,p1)`, `InputRead(b1)` issues `{kind:"i", rev:1}`, `InstallOk(b2,p1)`, then `UseICursor(b2)` presents b1's cursor.

1. instance.rs:362-369 compares only kind, revision and predicate. Both backends are at revision 1, so b2 accepts a continuation from a different owner.
2. With diverging data this silently skips or duplicates rows.
3. The same happens between an original backend and a restored copy at the same revision.
4. The rejection message ("does not match the live owner") and docs/bounded-reads.md:58 promise owner binding. `QueryCursor` is never accepted cross-owner, because the owner check at bounded.rs:125 holds in every configuration.

**Fix:** add `owner: String` to `InputCursor`, set it from the live `Runtime.identity` when issuing (instance.rs:388), and compare it at instance.rs:362-369. The Backend needs a small accessor for the identity.

### Issue 4: `observed_health` failure does not stop acknowledgements
Trace (standalone): `InstallOk(b1,p1)`, `ObservedHealth(b1)` (`try_wait` → `Err` for a live child; health becomes "failed"), then `ApplyCommitted(b1,{},FALSE)` acked with revision 2.

1. lib.rs:270-278 sets `failed` but keeps `runtime = Some`.
2. `apply_inner` checks only `runtime.is_none()` (lib.rs:461-463), and so do `read_runtime`, `count_rows` and `query_typed_bounded` (bounded.rs:123).
3. With no instance guard (instance_id None, or direct `Backend` use), a backend reporting "failed" keeps acknowledging transactions and accepting cursors. `export_inputs` and checkpoints stay refused. The invariant "`failed` ⇒ no acknowledgement" that every other path maintains is broken. Hosted mode is protected by instance.rs:152-158; `BackendRuntime.cfg` passes.

**Fix (either):**
- (a) Make `observed_health` also drop the runtime (`self.runtime = None`), as every other failure path does. The code then satisfies `failed ⇒ runtime = None`.
- (b) Add `if self.failed { return Err(..) }` to `apply_inner`, `read_runtime`, `count_rows` and `query_typed_bounded`. This is what `FixFailedGuard` models.

## Other checked facts (hold in every config)
- The revision is never bumped before the ack. Retained inputs never change before the native commit. A lost ack (I/O before or after commit, child death, oversized deltas) leaves `facts` and `revision` unadvanced and the backend not ready.
- A failed replacement (compile, start, replay reject or I/O) leaves the prior program usable with the same owner and revision.
- A reinstall always changes the owner and bumps the revision, so no `QueryCursor` survives it. The revision never repeats inside one backend, so stale same-backend cursors of either kind are always rejected.
- Standalone: a successful reinstall clears `failed` (lib.rs:419) and replays only acknowledged facts. That is allowed by design and does not violate `RetainedMatchesNative`, because the new child's `native = facts`. Hosted: a failed instance stays failed forever (`HostedPoisonSticky`).
