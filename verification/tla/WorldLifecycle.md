# WorldLifecycle: TLA+ model of `src/worlds.rs`

`WorldLifecycle.tla` models the world lifecycle state machine: `WorldManager`, its start thread, `ProcessControl` process groups, `world.json` persistence, and owner exit and recovery. Every action cites the Rust lines it models.

Run TLC from this directory:

```
java -XX:+UseParallelGC -cp /tmp/claude-0/tla2tools.jar tlc2.TLC -workers auto -config <cfg> WorldLifecycle.tla
```

`-deadlock` is not needed: the `Done` action is an explicit terminal stutter.

## What is modelled

**Atomic steps**

The whole-manager lock (`&mut self`, or `Mutex<WorldManager>` in `worlds_socket/mod.rs:88`) is the only guard of per-world state. So each manager method is one atomic action:

| Action | Code | Transitions |
|---|---|---|
| `Start` | `start_with` `:815-938` | T3 and T3f |
| `Stop` | `stop` `:939-968` | T9, T10, T11 |
| `Status` | `status_with` `:992-1134` | T4–T8, test copy, T12, `persist_if_changed` |
| `DropManager` | `Drop` `:1443-1459` | |

`StopAll` (`:1432-1441`) takes only the controls lock. The controls lock is held by `start_with` from `:818` to `:936`, so `StopAll` cannot fall inside `Start`, and it commutes with `Stop`'s controls section.

**The start thread** (`:909-935`)

It takes neither lock. It is split at every process interaction:

- `ThrSpawnComp`: spawn the compiler driver (`lib.rs:384-391`).
- `ThrTrackComp`: `track` (`processes.rs:44-48`). It inserts the pid, then kills it if the control is already stopped.
- `ThrWaitComp`: `wait` and `drop(group)` (`lib.rs:393-394`), then either `Err` or spawn the runtime (`lib.rs:401`, `:87`).
- `ThrTrackRt`: `track` the runtime (`lib.rs:88`).
- `ThrReplay`: the replay exchange (`lib.rs:407-417`). It succeeds only if the child is alive; otherwise the Runtime is dropped.
- `ThrFinish`: `run_scenarios`, `keep_world`, then `send` (`:914-934`). `Send` drops a live Runtime if the receiver is gone.
- `ThrPanic`: optional; unwinding gives `Disconnected`.

**Processes**

- Each process is keyed by `<<world, generation, kind>>` and carries `alive`, `tracked`, `ok` (exit status) and `owner`.
- `ProcessControl` is modelled as `ctlStopped[w][g]`:
  - `stop()` sets the flag, then kills every tracked group (`processes.rs:36-43`).
  - `Drop` of a Runtime or Group kills and untracks the process (`lib.rs:180-186`, `processes.rs:66-71`).
- The environment can:
  - `CompilerFinish`: the compiler exits by itself, successfully or not.
  - `Crash`: the native child dies (crash, or stdin EOF once it is orphaned).

**Persistence**

- `disk` is the (state, generation, error) of the last successful `world.json` write.
- `histLast` is the last in-memory history entry.
- Any write in `Start`, `Stop` or `Status` may fail, up to `MaxPersistFails` times.
- T3f leaves memory `failed` while disk keeps the old record.

**Owner exit and recovery**

`Exit(mode)` covers three ways the owner process can end:

| Mode | Code | Condition |
|---|---|---|
| `signal` | `stop_all` then `exit(143)` (`ddlog-worlds.rs:132-139`), or socket serve returning (`mod.rs:113`) | `S` must already be set |
| `drop` | stdio EOF after `Drop` (`ddlog-worlds.rs:158-175`) | `DropManager` must have run |
| `kill` | SIGKILL | always allowed |

On exit, start threads die. Children survive because each lives in its own process group (`processes.rs:77`).

`Recover` is `WorldManager::new` (T2, `:240-245`): starting, running and stopping records become `interrupted`, and generations continue from disk.

## Abstractions and omissions

- Errors are reduced to a cause tag: `none`, `persist`, `install`, `crash`, `worker`, `interrupted`.
- The test progress `Value` is reduced to a phase: building, failed, done.
- Scenario execution is one step.
- Not modelled, because they have no lifecycle effect:
  - the tailer thread, telemetry, captures, metadata and libraries;
  - `execute` exchange failures (they reach the model as `Crash` of the runtime).
- World creation and the `test()` removal path are not modelled. Worlds start out `created` and persisted.
- The persist of test progress at `:1041` is not modelled separately.
- Recovery persist failures are not modelled: `new()` would return `Err` and no owner would start.
- The embedding-library case where the process keeps running after `Drop` (R5) is not modelled. After `DropManager` the only owner step is `Exit`.
- Lock poisoning (R8) is not modelled.
- `-deadlock` is not used; `Done` stutters once the last owner has exited.

## Properties

These all **hold**. Invariants I1–I6 are guarded by `ManagerLive` (owner up and not dropped):

| Property | Meaning |
|---|---|
| `TypeOK` | type correctness |
| `I1` | `instance.is_some()` ⇔ state = running |
| `I2` | pending ⇒ starting or stopping |
| `I3` | never both an instance and a pending start |
| `I4` | a live thread ⇒ pending |
| `I5` | starting or stopping ⇒ pending |
| `I6` | stop_requested ⇒ stopping or stopped (holds only without panics) |
| `StaleThreadNeverInstalls` | the instance, the buffered result and the live thread all belong to `gen[w]` |
| `NoTwoLiveNativeChildrenPerWorldWithinOwner` | at most one live runtime per world per owner |
| `NoTwoLiveProcessesPerWorldWithinOwner` | stronger: at most one live process of any kind per world per owner |
| `DiskHistoryConsistency` | in-memory history last = disk, and memory ≠ disk only after an unretried failed write |
| `LiveInstanceIsTracked` | a live instance's child is tracked by the control in the map |
| `StoppingEventuallyStopped` | liveness: stopping ~> stopped (or owner gone) |
| `StoppedControlEventuallyKills` | liveness: a live process of a stopped control, owned by the live owner, eventually dies |

The liveness properties assume weak fairness of the start thread, `Status` and `Recover`.

These are **expected to fail**, and each has its own config:

| Property | Issue |
|---|---|
| `OwnerCleanExitLeavesNoOrphan` | R1 |
| `NoFailedAfterStopAll` | R4 |
| `NoTwoLiveProcessesPerWorldAcrossOwners` | R2 |
| `I6` with `AllowPanic = TRUE` | R10 / T8 |

## TLC results

TLC 2026.09.22, 4 workers. Raw outputs are in `traces/`.

| Config | Bounds | Result | States generated | Distinct | Depth | Time |
|---|---|---|---|---|---|---|
| `WorldLifecycle.cfg` | 2 worlds (instance plus test world with keep_world=false), MaxGen 2, 1 owner, 1 persist failure, kill allowed | **no error** | 4,617,087 | 936,465 | 44 | 51 s |
| `WorldLifecycle_Recover.cfg` | 1 instance world, MaxGen 3, 2 owners, 1 persist failure, kill allowed | **no error** | 849,101 | 244,976 | 37 | 15 s |
| `WorldLifecycle_Live.cfg` | 1 test world with keep_world=true, MaxGen 2, 2 owners, all invariants plus both liveness properties | **no error** | 166,462 | 54,560 | 28 | 21 s |
| `WorldLifecycle_R1.cfg` | 1 world, MaxGen 1, 1 owner | `OwnerCleanExitLeavesNoOrphan` violated | 83 | 39 | 5 | <1 s |
| `WorldLifecycle_R4.cfg` | same | `NoFailedAfterStopAll` violated | 514 | 148 | 12 | <1 s |
| `WorldLifecycle_R2.cfg` | 1 world, MaxGen 2, 2 owners, kill allowed | `NoTwoLiveProcessesPerWorldAcrossOwners` violated | 550 | 253 | 7 | <1 s |
| `WorldLifecycle_R10.cfg` | 1 world, panics allowed | `I6` violated | 70 | 36 | 5 | <1 s |

Running 2 worlds × MaxGen 2 × 2 owners exceeded 7M distinct states after 8 minutes, so recovery is covered by the separate one-world config.

**Checks that the properties are not vacuous.** Each mutant was run once and then discarded:

- Removing the `pending` gate from `Start` (`:821`) violates `I2`, and `StaleThreadNeverInstalls` in 3 states (Start, Start).
- Dropping fairness of the start thread violates `StoppedControlEventuallyKills`.
- Making `track()` not kill when the control is stopped (`processes.rs:46`) violates `StoppedControlEventuallyKills`.

## Counterexamples

**R1: process leaked when the owner exits between spawn and track. Real bug, narrow window.**

Trace in `traces/WorldLifecycle_R1.trace.txt`:

1. `Start(w1)`.
2. `ThrSpawnComp`: the compiler is spawned in its own process group (`lib.rs:391`), but `track` at `lib.rs:392` has not run yet.
3. `StopAll`: `S` is set and the tracked groups are killed. The compiler is not in the set yet (`worlds.rs:1432-1441`).
4. `Exit("signal")`: `std::process::exit(143)` (`ddlog-worlds.rs:138`).

The start thread dies before `track()`, so the compiler stays alive with `cleanExit` set. The same window exists for the runtime (`lib.rs:87-88`), and for the socket path `serve` → `stop_all` → return from `main` (`mod.rs:113`, `ddlog-worlds.rs:145`).

Without a process exit, `track`'s insert-then-check closes the race; the model confirms this, since `StoppedControlEventuallyKills` holds. Possible fixes: join or wait for start threads before exit, or register the pid before `spawn` completes.

**R4: after `stop_all`, a running world reports failed, not stopped. Real semantic mismatch; nothing leaks.**

Trace in `traces/WorldLifecycle_R4.trace.txt`:

1. `Start` and a full install, then `Status` reaps: running (`:1003-1007`).
2. `StopAll`: SIGKILL of the runtime. The world state is untouched (`:1432-1441`).
3. `Status`: crash detection (`:1049-1054`) gives `failed` with error "Native execution process exited unexpectedly".

`Drop` would have recorded `stopped` (`:1451`). In stdio mode, `exit(143)` persists nothing, so the next owner shows `interrupted`.

**R2: two live processes for one world across owners. Real, and documented as unsupported (`docs/worlds.md:102-103`).**

Trace in `traces/WorldLifecycle_R2.trace.txt`:

1. `Start`, `ThrSpawnComp`.
2. `Exit("kill")`: the flock is released (`worlds.rs:1513-1549`) and the compiler is orphaned.
3. `Recover`: the world becomes interrupted (`:240-245`).
4. `Start`: generation 2.
5. `ThrSpawnComp`: generation 1's and generation 2's compilers are both alive.

The within-owner version of this property holds.

**R10 / T8: a panic after stop is reported failed. Minor.**

Trace in `traces/WorldLifecycle_R10.trace.txt`:

1. `Start`.
2. `Stop`: stopping (`:960`).
3. `ThrPanic`: the Sender is dropped.
4. `Status`: `Disconnected` gives `failed` / worker error (`:1024-1027`), with `stop_requested` still true. That branch does not consult `stop_requested`, unlike `:998`.

**Not modelled: R8 (lock poisoning)**

With a poisoned controls lock:

- `stop` returns at `:954` after taking the instance, so an I1 violation ("running" with no instance) would follow;
- `stop_all` does nothing (`:1433`).

Modelling it would need a poison flag on the controls lock; left out.

## Expected properties that failed in the main config

None. Every property expected to hold passed without changing the model to fit it.
