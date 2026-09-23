# Registry.tla: the durable processor registry

This is a TLA+ model of `src/registry.rs`: the version files, the `current.json` pointer, the
lifecycle pointer and events, the create-new lock file, crashes, the operator, unlocked readers,
and `import_registry`. All line numbers below refer to `src/registry.rs`.

## What is modelled

| Model element | Code |
| --- | --- |
| `versions[p]` (a set that only grows) | `versions/<hash>.json`, written without replacing via `hard_link` (1421-1424) |
| `cur[p]` | `current.json`, replaced with `rename` (1419-1420) |
| `lcPtr[p]`, `lcEv[p][r]` | `lifecycle/current.json` and `lifecycle/revisions/<r>.json` (400-421) |
| `lock` | `.update.lock`: an exclusive create (1438-1453); Drop removes it by path without checking the owner (1455-1462) |
| `ChoosePublish` → `Acquire` → `PubCheck` → `WriteVersion` → `WriteCur` → `Release` | `publish_versioned` (244-276): lock at 255, `ensure_active` at 256, compare-and-swap on the current version at 258-264, then `publish_locked` (1011-1049) |
| `ChooseTransition` → `Acquire` → `TransCheck` → `WriteEvent` → `WritePtr` → `Release` | `transition` (366-423): version check 377, revision check 380, same-target no-op 383, uncommitted-event check 406-409, event written 412, **then** pointer written 413-421 |
| `ChooseCreate` → `Acquire` → `CreateDir` → `WriteVersion` → `WriteCur` | `create_versioned` and `create_locked` (210-220, 984-1001). Fork (278-297) has the same shape |
| `DecideNew` (unlocked) → `Acquire` → `ImportRecord`×2 → `ImportCommit` | `import_registry` (812-959): the new-processor decision at 905-910 **before** the lock at 922, `import_locked` at 766-805, and the unconditional pointer write at 944-956 |
| `Crash(w)` | any process dies at any step; a lock it holds stays on disk (header, lines 4-5) |
| `OperatorDeleteLock` | the operator removes the lock. With `UnsafeOperator=FALSE` this happens only when no live process holds it ("established writer absence", header 4-5) |
| `RStep` | the unlocked `lifecycle_snapshot` (543-584), whose steps are read current, read pointer, read event, re-read current, re-read pointer, then the archived check (577-579), followed by the second `current()` read in `get(None)` (302-311) |

### Abstractions
- **One file write is one step.** Each `atomic_json` call (write temp file, fsync, rename or
  hard link, fsync the directory: 1411-1432) is atomic, and a crash can fall between any two such
  calls.
- **A version is its content.** Versions are named by the SHA-256 of the definition (1283-1304),
  so the model identifies a version with its content value; lineage, provenance and compositions
  are omitted.
- **Clients read before they request.** A client reads `cur` and `lcPtr` without the lock when it
  builds a request, so stale expectations come from interleaving, not from arbitrary guesses.
- **Snapshots under the lock are consistent.** A lifecycle snapshot taken while holding the lock
  is modelled as one read, because no other writer can interleave.
- **Model bounds.** `MaxRev` bounds lifecycle revisions and `MaxCrashes` bounds crashes. `OpIds`
  and `ReadIds` limit which processors are targeted. `import_locked`'s version write and pointer
  write are merged into one step.
- **Reductions for TLC.** A VIEW drops leftover fields of idle processes and readers, which nothing
  reads again. SYMMETRY permutes writers.

## Properties

| Property | Meaning |
| --- | --- |
| `NoLostUpdate` | each pointer write (`current.json` or the lifecycle pointer) replaces a value its writer observed under the lock or wrote itself. The ghost set `clobbers` records every violation |
| `CurAlwaysPointsToExistingVersion` | `cur[p]` is always a version file that exists |
| `ArchivedEventMatchesCur` | the invariant enforced at 577-579: if archived, the event's version equals `cur` |
| `SnapshotLinearizable`, `GetLinearizable` | a reader's snapshot or `get(None)` result equals the true (current version, status) pair at some instant of its read interval. Each action appends the post-state pair to the reader's `seen` history |
| `VersionsImmutable` (action property) | version files are never removed, and lifecycle events never change once written |
| `OrphanBlocksTransitions` (action property, C4) | while an orphaned event exists (event r+1 written, pointer still at r, no live writer about to commit it), the lifecycle pointer never moves |
| `NoOrphanEvent` | a reachability witness for C4, expected to be violated |
| `OrphanEventuallyResolved` (liveness, under `FairSpec`) | C4 as liveness, expected to be violated |

## TLC results (TLC2 2026.09.22)

Unless a row says otherwise: Vals={v1,v2}, MaxRev=2, MaxCrashes=1, processor a exists at start,
and the foreign processor b has source versions <<v1,v2>> with source current v2. Runs used 4
workers, with `-metadir` pointing outside the repository.

| Config | Setup | Result |
| --- | --- | --- |
| `Registry.cfg` (core, safe) | Writers {w1,w2}, publish/archive/restore on a, create of c, crashes, safe operator, reader r1 on a | **All properties hold** (5 invariants, 2 action properties). 123,790,258 states generated, 28,722,870 distinct, depth 121, 9 min 42 s |
| `Registry_import.cfg` (safe) | Writer w1 publishing/archiving b, ONE importer i1 for b, reader r1 on b, crashes, safe operator | **All properties hold.** 448,159 states generated, 109,718 distinct, depth 57, 3 s |
| `Registry_C1.cfg` | w1 on b, importers {i1,i2}, no crashes | **`NoLostUpdate` violated**, depth 18 (5,144 generated / 1,806 distinct) |
| `Registry_C1_archived.cfg` | same setup | **`ArchivedEventMatchesCur` violated**, depth 24 (13,217 / 4,654) |
| `Registry_C3.cfg` | Writers {w1,w2} on a, `UnsafeOperator=TRUE`, no crashes | **`NoLostUpdate` violated**, depth 12 (3,636 / 1,764) |
| `Registry_C4_reach.cfg` | w1 on a, 1 crash | **`NoOrphanEvent` violated**, depth 6 (101 / 55) |
| `Registry_C4.cfg` | w1 on a, 1 crash, `FairSpec` | **`OrphanEventuallyResolved` violated**, lasso (1,939 / 839) |

A single run combining `Registry.cfg` and `Registry_import.cfg` (2 writers, 1 importer, a reader,
and operations on both a and b) passed more than 8M distinct states without an error before it
was stopped for time. The two parts touch different processors except through the shared lock,
and both parts hold.

## Counterexamples mapped to code

### C1: concurrent `import_registry` rolls back a published pointer (`Registry_C1.cfg`)
Setup: importers i1 and i2 bring in the same foreign processor b from a source whose versions are
<<v1, v2>> and whose current version is v2. Writer w1 can publish or archive b.

1. **States 2-3.** i1 runs `DecideNew` and sees that b has no `current.json` (905-910), so `new=TRUE`.
   It then acquires the lock (922).
2. **State 4.** i2 runs `DecideNew` and also sees b absent, so `new=TRUE`. Its acquire comes later.
3. **States 5-7.** i1 runs `import_locked`. For v1 it creates `current.json` pointing at v1 (792-803).
   Then it imports v2 and commits `current.json` to v2 (944-956).
4. **States 8-13.** w1 publishes b→v1 with `expected=v2`. The version check passes under the lock
   (258-264), and `current.json` becomes v1 (1040).
5. **States 15-18.** i2 finally acquires the lock (922). `import_locked` sees `current.json` exists, so
   it writes no pointer (780-784). Then `ImportCommit` at 944-956 rewrites `current.json` to v2,
   because i2's `new_processors` decision from step 2 is stale.

   **w1's publish is lost:** `clobbers={<<i2,b,"cur">>}`. Found at depth 18; 5144 states
   generated, 1806 distinct.

`Registry_C1_archived.cfg` runs the same race with w1 archiving b at v1 instead of publishing. The
lifecycle event is `[archived, ver v1]`, and then i2's commit sets `cur[b]=v2`, which violates
`ArchivedEventMatchesCur`. After that, every `lifecycle_snapshot` of b fails at 577-579 until an
operator repairs it. Found at depth 24; 13217 states generated, 4654 distinct.

**Suggested fix:** compute `new_processors` under the lock, i.e. move lines 905-910 after line
922. Better still, make the final pointer write conditional: write `current.json` only if it is
still absent, or still points at the first-record pointer this same import wrote in
`import_locked`. Writing the source's current pointer for a new processor inside `import_locked`
itself would also remove C2 (after a partial import, `current.json` names an arbitrary first
version).

### C3: the operator deletes a live writer's lock, causing a lost update (`Registry_C3.cfg`)
1. **States 2-5.** w1 runs `publish(a, v1, expected=v1)`. It acquires the lock, passes the version
   check, and writes the version file. It is now about to rename `current.json` (1040).
2. **State 7.** The operator deletes `.update.lock`, mistaking the slow w1 for a crashed writer.
3. **States 6 and 8-11.** w2 runs `publish(a, v2, expected=v1)`. It acquires the lock (1441), sees
   `cur=v1`, and the version check passes. It writes `current.json := v2`.
4. **State 12.** w1 writes `current.json := v1` without re-reading (1040-1049). **w2's update is
   lost.** Afterwards w1's Drop will also delete w2's lock file (1455-1462).

Found at depth 12; 3636 states generated, 1764 distinct.

**Suggested fix:** use a kernel lock (`flock`/`fcntl`) on a file that persists, so a crash releases
it automatically and no operator deletion is needed. At minimum, make Drop remove the lock only if
it still contains this process's pid and a random nonce written at acquire time. Also re-read and
compare `current.json` immediately before the rename. That check is not atomic, but it shrinks the
window.

### C4: an interrupted transition blocks the lifecycle permanently (`Registry_C4_reach.cfg`, `Registry_C4.cfg`)
- **Reachability** (`NoOrphanEvent` violated, depth 6; 101 states generated, 55 distinct): w1
  archives a. It passes `TransCheck`, writes `revisions/…01.json` (412), then crashes before the
  pointer write (413). The lock is left behind.
- **Liveness** (`OrphanEventuallyResolved` violated under `FairSpec`: weakly fair writers plus
  strong fairness on attempting archive/restore; 1939 states generated, 839 distinct; lasso back
  to state 19). The lasso runs:
  1. w1 publishes a→v2 and archives it (lifecycle revision 1).
  2. w1 starts a restore, writes event 2 (412), and crashes before the pointer write.
  3. The operator, having established writer absence, deletes the stale lock.
  4. From then on, every publish aborts because a is archived (256), and every restore aborts at
     406-409 ("Uncommitted lifecycle event 2 already exists").

  **The processor is stuck archived forever.** `OrphanBlocksTransitions` holds in every
  configuration checked.

**Suggested fix:** under the lock, treat an uncommitted `revisions/<r+1>.json` as garbage from a
crashed writer that held the lock (the lock guarantees nobody is still writing it). Delete it or
rename it aside, then continue. Alternatively, fold the event into the pointer file so the
transition is a single rename.

Full TLC output for each counterexample is in `traces/Registry_*.out`.
