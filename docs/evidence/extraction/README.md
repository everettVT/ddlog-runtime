# Extraction verification

The extracted host passed all 98 complete connected-component comparisons against the independent BFS oracle, including two bridges, disconnect/reconnect, edge additions/deletions, alternate support, isolates, signed IDs and full retraction. Cleanup completed without errors.

This run reused a retained official DDlog native artifact after verifying generated source, declaration, implementation and executable hashes. It performed **one native activation, zero fresh native compilations, and zero provider calls**. `build.log` records that reuse. It does not establish a portable fresh toolchain build.

`native-smoke.json` binds the extracted host, source/native hashes and observations. `rpc.jsonl` contains 198 synthetic tool exchanges; `snapshots.jsonl` contains all 98 independent expected/actual partitions. The raw receipt hash is retained; only its incidental local temporary-directory field was removed and observations were separated. No executable, private path, credentials or provider workload is published.

`verification.json` records 44 transport-free Rust tests, 46 all-feature Rust tests, 56 controlled Python tests, the independent consumer, syntax package verification, package-asset inspection, and source hashes. It also records the initial test-fixture correction and classified local socket permission rerun. These new results are separate from historical upstream evidence.
