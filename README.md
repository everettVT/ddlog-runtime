# DDlog Runtime

A typed incremental program runtime extracted from [Lemmalog](https://github.com/everettVT/lemmalog). Author rules, register immutable program versions, compose their public relations, apply input transactions, and inspect maintained outputs through a Rust library or MCP.

The original Lemmalog agent-memory product remains in its own repository. This runtime contains no memory interpreter, episode store, extraction policy, retrieval, or embeddings.

| Package | Responsibility |
| --- | --- |
| `lemmalog-syntax` | Dependency-free source parser, AST, `Term` and `AggFn` |
| `ddlog-runtime` | Typed lowering, program ownership, registry, composition, native execution and registered requests |
| Optional `mcp` feature | Existing stdio server and shared Unix host/bridges |
| Optional Python worker | External inference through the existing claim/complete protocol |

The source language accepts positive recursion, safe stratified negation, joins, projections and comparisons over explicitly declared `int` and `string` fields. Negated variables must be bound by positive atoms. Negative cycles and cycles through native transformers are rejected before compilation. A vetted Large-Star/Small-Star operator performs native iterative connected components. Aggregates, arithmetic, clock builtins and inline facts remain unsupported even though the shared parser recognizes them.

## Library

```rust
use ddlog_runtime::{Backend, ProgramInstance};
use serde_json::json;
use std::collections::BTreeMap;

fn main() -> Result<(), String> {
    let backend = Backend::new("/absolute/build/session-1".into(),
                               "/absolute/scripts/build-ddlog.sh".into());
    let mut program = ProgramInstance::new(backend, BTreeMap::new(), None, None);
    program.execute("lemmalog_install_rules", &json!({
        "rules": "visible(X) :- item(X).",
        "schemas": {
            "item": {"input": true, "fields": ["string"]},
            "visible": {"input": false, "fields": ["string"]}
        }
    }))?;
    program.execute("apply_changes", &json!({"changes": [
        {"op": "insert", "predicate": "item", "values": ["hello"]}
    ]}))?;
    let result = program.execute("lemmalog_query", &json!({"predicate": "visible"}))?;
    Ok(())
}
```

`ProgramInstance::execute` takes an operation name and JSON arguments and returns a result or semantic error. It has no JSON-RPC envelope or connection lifetime. MCP delegates to this same admission path, including immutable pins, exported ports and registered request restrictions. A caller owns and drops each independent instance. Give independent backends distinct build directories.

`Backend` is the lower-level compilation/execution interface. `Backend::install_composition(&registry, &manifest)` validates exact registry pins and typed bindings, then installs the composition and returns its `CompositionResolution` with generated input/output relation names. Use `ProgramInstance` for public-interface enforcement or registered operations. Existing MCP queries and deltas contain DDlog row text; library callers can use `Backend::query_typed` and `export_inputs` for typed rows. `why` returns direct rule-variable witnesses, not recursive proof trees or confidence/provenance.

Library callers can explicitly save and restore pure-program local checkpoints. See [checkpoint contracts](docs/checkpoints.md). Compatible output-schema changes recompile and replay retained inputs; input schemas stay fixed when data is retained.

A runnable library example is [`examples/program.rs`](examples/program.rs). Installation invokes native compilation; pure parsing, lowering and registry validation do not. The workspace does not require the MCP feature for library use.

## MCP

```sh
cargo build --locked --features mcp --bin lemmalog-ddlog-mcp
export LEMMALOG_DDLOG_BUILD="$PWD/scripts/build-ddlog.sh"
export LEMMALOG_DDLOG_WORKDIR=/absolute/writable/build-directory
export DDLOG_HOME=/absolute/ddlog-distribution
./target/debug/lemmalog-ddlog-mcp
```

Configure the pinned native build environment described in [building](docs/building.md) before installation. No compiler or dependencies are installed implicitly.

For independently attached same-user clients:

```sh
lemmalog-ddlog-mcp host --socket /absolute/private/owner.sock --descriptor /absolute/private/owner.json
lemmalog-ddlog-mcp connect --descriptor /absolute/private/owner.json
lemmalog-ddlog-mcp stop --descriptor /absolute/private/owner.json
```

The private directory must be owned by the operator with mode `0700`. The owner persists across bridge disconnects and serializes semantic operations. Set `LEMMALOG_PROCESSOR_REGISTRY` to a registry directory for durable definitions; set `LEMMALOG_AGENT_OPERATIONS` to an operator-owned operation registry when using external inference.

Existing binary, tool, server and environment names are deliberately retained. This server is **not wire-compatible with Lemmalog's memory MCP server**. See [operations](docs/operations.md), [external inference](docs/inference.md), and [extraction provenance](PROVENANCE.md).

## State and limits

Registry definitions and lifecycle records are durable. Graph state remains memory-only until an explicit library checkpoint succeeds. The MCP host does not automatically checkpoint or restore; reconnecting only preserves an existing live owner. Pure programs can restore a checkpoint into a fresh library backend and recompute their outputs. Registered operations and imported native operators cannot use checkpoint format 1. Compilation is blocking. Host stop cancels local compiler/runtime process groups; per-operation native timeouts, WAL replay, and automatic crash recovery are not implemented. Native execution runs with operator privileges.

One registered operation consumes/returns strings, with explicit submission, claim and completion. Providers run outside rule evaluation. Operation-bearing programs cannot participate in composition or contain typed operators. There are no automatic provider retries, background scheduling, leases, or exactly-once guarantees.

## Verification

```sh
cargo fmt --all --check
cargo test --locked --workspace
cargo test --locked --workspace --all-features
cargo build --locked --features mcp --bin lemmalog-ddlog-mcp
python3 -m unittest discover -s tests -p 'test_*.py' -v
```

These tests need Rust and Python 3.11+, but no DDlog compiler or provider. Python host/MCP tests use explicitly simulated graph fixtures. [Native acceptance](docs/building.md#native-acceptance) is a separate operator-configured step. Compatibility fixtures captured before extraction check old registry records, content hashes, generated source, bundled native source and MCP schemas.

MIT licensed; the upstream copyright notice is retained in [LICENSE](LICENSE).
