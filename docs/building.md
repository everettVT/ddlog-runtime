# Building and native execution

The Rust host and the generated DDlog program use separate toolchains. Build the host with a current Rust toolchain and the checked-in workspace lockfile. It depends on the syntax crate, serde/serde_json, SHA-256, and Unix libc. It does not link DDlog, Differential or Timely into the host binary.

Native installation runs `scripts/build-ddlog.sh SOURCE OUTPUT`. The trusted operator configures that script path; authored programs cannot choose executables or upload native code. The driver invokes the DDlog compiler, enters the generated `program_ddlog` project, builds `program_cli`, and copies it to the requested output.

## Reproducible native installation

Prerequisites: Python 3.12+, `curl`, Git, a current Rust installation managed by `rustup`, and a C/C++ build toolchain (Xcode command-line tools on macOS; build-essential on Linux). Supported compiler release targets are macOS and Linux x86_64. The macOS compiler is x86_64 and needs Rosetta on Apple Silicon. Windows and Linux ARM are not covered by this installer.

Build the modern host **before** loading the native environment:

```sh
cargo build --locked --features mcp --bin lemmalog-ddlog-mcp
python3 scripts/bootstrap-native.py /absolute/new/ddlog-native
(
  . /absolute/new/ddlog-native/env.sh
  python3 scripts/test-native-install.py /absolute/new/smoke-evidence
)
```

The bootstrap downloads the official DDlog v1.2.3 archive and verifies its checked-in SHA-256, installs Rust 1.65.0 into the dedicated directory, generates a tiny native project, and uses modern Cargo to acquire and vendor the complete locked dependencies. Cargo verifies registry checksums and exact Git revisions. The checked-in `native/program.Cargo.lock` and `native/star.Cargo.lock` pin the same third-party dependencies, including Differential/Timely 0.12 forks. Their generated workspace packages differ for the bundled Star operator. The historical release's incomplete vendor directory is not used.

The installer requires a new directory and retains failed installations for diagnosis. Retry with another directory after addressing the reported failure. Budget several gigabytes for Rust, sources and native build artifacts. Installation needs network access; subsequent generated-program builds run with `--offline --locked`. No prepared vendor tree or previously compiled program is needed. `receipt.json` records archive and lockfile identities; `env.sh` configures the shipped build driver. Keep that installation at its chosen absolute path. Native builds sharing its target directory should run sequentially.

The smoke test retains the actual MCP requests/responses, generated source, executable hashes and build logs. It tests a fresh program compilation, insertion, visible output and retraction. The `Native installation` workflow repeats this from public inputs on a fresh Linux runner and also compiles the bundled Star operator. It runs for packaging PRs or by manual dispatch; simulated contract tests remain separate.

### Existing operator-managed environments

The driver still accepts `DDLOG_HOME`, `DDLOG_CARGO`, `RUSTC`, `CARGO_HOME`, `CARGO_TARGET_DIR`, `DDLOG_CARGO_CONFIG`, `DDLOG_OFFLINE` and `DDLOG_CARGO_LOCK`. A supplied `DDLOG_CARGO_LOCK` takes precedence over bootstrap's `DDLOG_LOCK_DIR`; otherwise the driver selects the ordinary or Star lock from that directory. The repository root lockfile is for the modern host, not generated programs. Use absolute paths and scope native environment variables to the native server/build process, not the modern host build.

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
