# Building and native execution

The Rust host and the generated DDlog program use separate toolchains. Build the host with a current Rust toolchain and the checked-in workspace lockfile. It depends on the syntax crate, serde/serde_json, SHA-256, and Unix libc. It does not link DDlog, Differential or Timely into the host binary.

Native installation runs `scripts/build-ddlog.sh SOURCE OUTPUT`. The trusted operator configures that script path; authored programs cannot choose executables or upload native code. The driver invokes the DDlog compiler, enters the generated `program_ddlog` project, builds `program_cli`, and copies it to the requested output.

The established native environment is official **DDlog v1.2.3**, its pinned Differential Dataflow **0.12** / Timely **0.12** forks, and **Rust 1.65.0** for generated code. This extraction does not modernize or bundle that environment. The historical release's vendor directory was incomplete; the original runs used an operator-prepared complete vendor tree. Supply and verify your own complete dependencies/configuration.

```sh
export DDLOG_HOME=/absolute/ddlog-v1.2.3
export DDLOG_CARGO=/absolute/rust-1.65.0/bin/cargo
export RUSTC=/absolute/rust-1.65.0/bin/rustc
export DDLOG_CARGO_CONFIG=/absolute/native-cargo-config.toml
export DDLOG_CARGO_LOCK=/absolute/native-Cargo.lock
export DDLOG_OFFLINE=1
export CARGO_HOME=/absolute/native-cargo-home
export CARGO_TARGET_DIR=/absolute/native-target
```

Set these for the MCP server/native driver process, not the shell building the modern host. `DDLOG_CARGO_CONFIG` and `DDLOG_CARGO_LOCK` are optional operator overrides. Offline mode uses `--offline --locked`. A supplied native lock must describe the generated package named `program`; Star programs also need the generated `types__lemmalog_star` workspace package and dependency edges. The root repository lockfile is the host lock, not that native lock.

The library bundles `src/star/lemmalog_star.dl` and `.rs` as text and writes them beside generated source when selected. The generated source binds their hashes. This native Rust code compiles in DDlog's generated context, including `Weight` and `ddlog_std`; it is not an independently compiled workspace crate.

Use distinct writable build roots for independent owners. Serialize native builds if they share a Cargo target directory. Retain source, compiler/build identity, executable hashes and logs separately from the immutable authored definition identity. A new definition's validation receipt proves pure validation only; native compilation happens during installation. Failed candidate compilation preserves the existing active program.

## Native acceptance

After configuring the host and build driver, use the provider-free drivers:

```sh
python3 scripts/test-ddlog-mcp.py
python3 scripts/test-agent-requests.py
python3 scripts/test-shared-instance.py
python3 scripts/test-composition.py
python3 scripts/test-large-small-star.py
```

Set `LEMMALOG_DDLOG_MCP` to the host executable. These are separate native acceptance runs and may compile multiple graphs; run them sequentially with sufficient disk space. The registered-request driver uses a labeled mock external worker and makes no model calls. The Star driver compares complete partitions with an independent BFS oracle, including additions, deletions, alternative edge support and retraction.

`NATIVE_ARTIFACT_MODE=verified-reuse` labels a test run whose operator-supplied driver verifies source and executable hashes and reuses an existing native artifact. The default is `compile`. Reuse proves execution of the extracted host against that exact native graph; it does not prove a fresh compilation or portable toolchain setup. Receipts count activation separately from compilation.
