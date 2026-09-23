# Formal verification

Two complementary efforts:

- **[`lean/`](lean/README.md)** proves what composed programs *mean*. It covers
  isolation of node-private relations, preservation of public interfaces
  through nesting, and equivalence of lowering versions 1 and 2. These are
  machine-checked Lean 4 theorems over a model of `src/composition.rs`.
- **`tla/`** model-checks the concurrent and failure-prone *runtime* protocols
  with TLC. Each spec cites the Rust lines of every action. Each has a
  configuration where its safety and liveness properties hold. For every
  suspected defect there is a separate configuration where TLC produces a
  counterexample trace.

Both are models. They check the design as transcribed from the code, within
the stated bounds; they do not certify the Rust implementation or the DDlog
engine. The per-model write-ups list what is abstracted.

## Running

```sh
# Lean (toolchain pinned in lean/lean-toolchain)
cd verification/lean && lake build

# TLA+ (TLC from https://github.com/tlaplus/tlaplus/releases, Java 11+)
cd verification/tla
java -XX:+UseParallelGC -cp tla2tools.jar tlc2.TLC -workers auto \
  -metadir "$(mktemp -d)" -config WorldLifecycle.cfg WorldLifecycle.tla
```

Give each TLC run its own `-metadir`: concurrent runs that share the default
`states/` directory interfere with each other.

## TLA+ models

| Model | Code | Holds (main configs) | Counterexample configs |
| --- | --- | --- | --- |
| [`WorldLifecycle`](tla/WorldLifecycle.md) | `worlds.rs`, `processes.rs` | Lifecycle invariants I1–I6; a stale start thread never installs; at most one live native child per world per owner; stop eventually kills | `_R1` orphan on exit between spawn and track; `_R4` failed instead of stopped after `stop_all`; `_R2` duplicate processes across owners after SIGKILL; `_R10` panic after stop reported as failed |
| [`BackendRuntime`](tla/BackendRuntime.md) | `lib.rs`, `instance.rs`, `bounded.rs`, `checkpoint.rs` | Retained inputs match native state when ready; cursor soundness; revision only on ack; failed replacement preserves prior program; checkpoint/restore sound | `Bug1` native error text accepted as deltas; `Bug2` an oversized read poisons a healthy instance; `Bug3` input cursor accepted by another instance; `Bug4` apply acknowledged after `observed_health` failed; `Fixed*` all hold with the proposed fixes |
| [`Registry`](tla/Registry.md) | `registry.rs` | No lost update; pointer always names an existing version; archived event matches pointer; readers linearizable | `_C1` concurrent `import_registry` rolls back a publish / breaks the archived invariant; `_C3` deleting a live writer's lock loses an update; `_C4` a crash between lifecycle event and pointer blocks transitions forever |
| [`Requests`](tla/Requests.md) | `operations.rs`, `instance.rs` | At most one output per request; no completion without claim; claims only fresh; `fresh` flag matches `agent_result`; uncertain settlement never permits repeating provider work | `_C5` standalone duplicate completion acknowledged after runtime death; `_C8` a non-claimant can settle another worker's claim |

TLC output for the counterexamples is in [`tla/traces/`](tla/traces).
