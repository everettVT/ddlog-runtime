# Hardening baseline — September 6, 2026

Assessed merged revision `f763612129a0275e53b34869a428c12c1a5bf7a9` in an
isolated checkout. Active bounded-read work is excluded and must be assessed
when available. This is a measured baseline and targeted cleanup, not a
production certification or a claim that all hotspots have been eliminated.

## Changes and validation

- Separated registry operations and pinned activation from semantic dispatch.
  Existing health/pin admission guards remain in the shared dispatch path.
- Added registered-request contracts for claim requirements, stale completion,
  identical/conflicting completion, and lost native acknowledgment. They test
  the real Rust admission implementation with explicitly simulated transport.
- Corrected syntax documentation that implied aggregate execution support.
- Added the runtime specification with explicit tests and unsupported guarantees.
- Simplified three Clippy findings without changing public APIs or generated code.
- Final instrumented Rust suite: 69 passed, one native-compiler test ignored.
- Final Python suite: 58 passed. Unix socket access required an escalation;
  the initial sandbox denial occurred before any runtime test executed.
- Strict library Clippy passed. All-target Clippy additionally reports the
  existing boxed-closure type complexity in `tests/memory_runtime.rs`; that file
  is owned by ongoing checkpoint work and was not modified here.

## Measurement

Tools: rustc 1.94.1, cargo-llvm-cov 0.9.1, Lizard 1.24.0. Runtime source line
coverage is 2795/3067 = **91.13%**, up from 2755/3065 = **89.89%**. These totals
exclude integration tests, the separate syntax crate and generated/native engine
code. Host subprocess profiles from the Python suite are included. Forced process
termination can omit profiling data, so an uncovered function is a review target,
not proof that no test executes it.

The diagnostic uses Lizard's Rust cyclomatic complexity and executable-line
coverage in each function's source range from LLVM LCOV. Formula:
`CC² × (1 − coverage)³ + CC`. This is a line-based CRAP estimate, not branch
coverage or formal correctness evidence. No hard score gate has been chosen.
The main dispatch estimate changed from 163.2 (CC106) to 49.4 (CC45); responsibility
extraction reduces local complexity but does not remove total system complexity.

| Remaining hotspot | CC | Line coverage | CRAP estimate |
| --- | ---: | ---: | ---: |
| `src/instance.rs::execute` | 45 | 87.1% | 49.4 |
| `src/host.rs::standalone` | 6 | 0.0% | 42.0 |
| `src/lower.rs::lower_clauses_with_operators` | 41 | 92.3% | 41.8 |
| `src/instance.rs::execute_registry` | 41 | 93.3% | 41.5 |
| `src/instance.rs::install_processor` | 22 | 68.3% | 37.4 |
| `src/composition.rs::manifest` | 35 | 92.4% | 35.5 |
| `src/host.rs::host` | 32 | 88.8% | 33.4 |
| `src/host.rs::connection` | 26 | 87.9% | 27.2 |

## Reproduction

Install cargo-llvm-cov 0.9.1 and Rust llvm-tools-preview. Run Rust coverage with
`cargo llvm-cov --locked --workspace --all-features --no-report`. Run the Python
suite against the instrumented `target/llvm-cov-target/debug/lemmalog-ddlog-mcp`
by setting `LEMMALOG_DDLOG_MCP`, with `LLVM_PROFILE_FILE` pointing into that coverage
target using distinct process/module placeholders `%p-%m.profraw`. Then export
`cargo llvm-cov report --lcov --output-path coverage.lcov`. In an environment with
Lizard 1.24.0, run `python scripts/complexity_coverage.py coverage.lcov`.

Do not report Rust-only coverage as transport coverage. Do not report this fixture
suite as native compilation or formal validation of a Lean-to-dataflow compiler.

## Remaining work before a broad clean bill

Reconcile the incoming bounded-read branch; review compiler, composition and host
hotspots in context rather than splitting them merely to reduce a score. Make
native compiler validation available as explicit evidence, and decide which
coverage/complexity checks belong in CI after the baseline stabilizes. The current
library has local pure checkpoints, not integrated generic Iceberg persistence.
No world, ECS, mission, deployment or external-effect recovery features were added.
