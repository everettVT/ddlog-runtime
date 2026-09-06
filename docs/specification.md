# Runtime contract

This document describes the supported runtime, not planned world or storage
abstractions. The library and MCP expose the same program admission rules;
transport does not define program semantics. Focused guides below describe
wire shapes and operational prerequisites.

| Boundary | Required behavior | Executable evidence |
| --- | --- | --- |
| Language | Explicit `int`/`string` schemas; positive recursion and safe stratified negation; reject unsupported constructs and negative cycles before compilation | `tests/ddlog_lowering.rs`, `tests/recursive_lowering.rs` |
| Definitions | Immutable versions preserve identity and lineage; conditional pointer and lifecycle updates reject stale callers | `tests/processor_registry.rs` |
| Composition | Resolve exact versions, isolate private names, validate bindings, and preserve public interfaces through nesting | `tests/processor_composition_registry.rs` |
| Activation | Compile a candidate before replacement; compatible retained inputs replay; failed replacement preserves the prior usable program | `tests/memory_runtime.rs` |
| Input changes | Validate the complete input transaction before execution; acknowledged state changes only after completion; uncertain native failure disables continued use | `tests/memory_runtime.rs` |
| Registered requests | Claim before completion; preserve late results with explicit freshness; identical completion is idempotent; conflicting completion fails; uncertain settlement never implies permission to repeat provider work | `tests/registered_requests.rs` |
| Shared access | Connections share one owner; disconnect does not stop it; pinned versions and exported ports remain enforced | `tests/test_shared_host.py`, `tests/upstream_compatibility.rs` |
| Checkpoints | Explicit pure-backend checkpoint; integrity-checked restore into a fresh backend; reconstruct outputs from acknowledged inputs | `tests/memory_runtime.rs`, [checkpoint contract](checkpoints.md) |
| Compatibility | Preserve previously published definition hashes, native source fixtures, and MCP tool schemas unless a deliberate compatibility change is declared | `tests/upstream_compatibility.rs` |

## What an acknowledgment establishes

A completed input operation means the native program acknowledged its transaction.
It does not mean that inputs are durable, an external provider completed, or an
Iceberg catalog published anything. Program installation version and backend
revision are distinct; callers must not use a version number as a transaction ID.

An explicit checkpoint has its own durability outcome. Directory-sync failure
after replacement is uncertain publication and requires inspection before retry.
Checkpoint integrity is corruption detection, not authentication. Caller metadata
does not automatically reconstruct registry pins, public interfaces, or provider
admission state. See [checkpoints](checkpoints.md) for supported contents and limits.

## Deliberate limits

- The parser recognizes more syntax than the runtime supports. Parsing is not
  compilation, and compilation is not a mathematical correctness proof.
- Registered-operation programs do not compose with ordinary program nodes.
  Imported native operators and registered operations cannot use checkpoint
  format 1. Ordinary inference state is session-local.
- One host owns one graph instance. No fleet scheduling, world semantics,
  automatic crash recovery, distributed transaction, or provider exactly-once
  guarantee is supplied.
- Generic Arrow/Iceberg persistence is not integrated. Separate experiments are
  evidence for their scoped scenarios, not shipped runtime guarantees.
- `why` provides direct rule witnesses, not a Lean proof, recursive provenance,
  or a certificate for a source-to-dataflow compiler.

## Validation scope

Default Rust and Python suites validate runtime contracts using controlled native
transport fixtures. They do not execute the actual DDlog compiler or certify the
upstream engine. [Native acceptance](building.md#native-acceptance) is a separate
configured run. Record whether native evidence is a fresh compilation or reuse of
a hash-verified executable.

Coverage must include the instrumented MCP binary exercised by Python, otherwise
host coverage is understated. Generated native sources, Rust host code, syntax
code, and Python worker coverage are separate populations. Report the population
and skipped tests with any percentage. Complexity/coverage scores prioritize
review; they do not prove correctness or justify splitting code solely to improve
a metric.
