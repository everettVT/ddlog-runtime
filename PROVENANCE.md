# Extraction provenance

This repository extracts the typed program runtime from [everettVT/lemmalog](https://github.com/everettVT/lemmalog), source tree at commit `199a6cd87bbc16e5e5f6069bcef80edebd9c8c9f` (equivalent merged tree `682e726c256496dc5eb2cd71792780bf146551c3`). The upstream MIT license and `Copyright (c) 2026 Jordy` notice are preserved in both packages.

| Upstream source | Extracted location |
| --- | --- |
| `src/ast.rs` | `crates/syntax/src/ast.rs` |
| `src/intern.rs` syntax-only `AggFn` and `Term` | `crates/syntax/src/lib.rs` |
| `src/ddlog/mod.rs` and runtime modules | `src/lib.rs` and `src/` |
| `src/ddlog/mcp.rs` semantic state/dispatch | `src/instance.rs` |
| `src/ddlog/mcp.rs` JSON-RPC/schemas | `src/mcp.rs` |
| `src/bin/lemmalog-ddlog-mcp.rs` | Same binary path/name |
| Native Star declaration/implementation | `src/star/` with unchanged bytes |
| Runtime-only Rust/Python tests and build scripts | `tests/`, `scripts/` |
| Optional inference worker | `scripts/lemmalog_inference_worker.py` with unchanged bytes |

Memory source is not moved or made dependent on this repository. Its evaluator, runtime values/interner, annotations, AgentMemory, canonicalization, episodes, snapshots, retrieval, embeddings, memory MCP/CLI/REPL and examples remain upstream. Sharing syntax does not imply shared backend semantics or a memory migration.

Before the package move, five synthetic saved definitions and their raw registry files, MCP schemas and nested generated source were captured from the unchanged upstream server. `tests/fixtures/upstream/manifest.json` records its hash and source digests. The capture used a simulated build driver solely to obtain generated source; it is not native execution evidence. Compatibility tests reopen those records, retain content versions, regenerate identical nested source, compare native text hashes and compare MCP schemas.

The historical settlement fixture contains only four already-public successful completion envelopes from the original registered-inference run. Its upstream source is [rpc.jsonl](https://github.com/everettVT/lemmalog/blob/199a6cd87bbc16e5e5f6069bcef80edebd9c8c9f/docs/evidence/registered-inference/rpc.jsonl). These are regression inputs, not new provider calls. The composition fixture is copied from that tree's `docs/evidence/composition/fixture.json`.

No tool names, server identity, registry format, content-hash algorithm, generated relation names or native implementation bytes are intentionally changed. The runtime's semantic dispatcher is now public and independent of its optional transport. Changes include package/import boundaries, that separation, documentation and focused compatibility coverage. Linux CI also exposed an inherited pipe bridge stall: socket-to-stdout forwarding now uses an explicit read/write/flush loop instead of `io::copy`, so responses arrive while connections remain open. Framing, tool behavior, stdin half-close and instance lifetime are preserved.
