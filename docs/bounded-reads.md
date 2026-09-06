# Bounded output reads

The lower-level Rust library supports pages from one maintained native output:

```rust
use ddlog_runtime::{Backend, BoundedQuery};
use serde_json::json;
use std::collections::BTreeMap;

fn read(backend: &mut Backend) -> Result<(), String> {
    let mut query = BoundedQuery {
        filters: BTreeMap::from([(0, json!("selected-key"))]),
        max_rows: 20,
        max_bytes: 64 * 1024,
        continuation: None,
    };
    loop {
        let page = backend.query_typed_bounded("visible", &query)?;
        // Consume this page before requesting another; retaining every page
        // would move an unbounded snapshot into the application.
        println!("{} rows, {} JSON bytes", page.rows.len(), page.bytes);
        match page.continuation {
            Some(cursor) => query.continuation = Some(cursor),
            None => break,
        }
    }
    Ok(())
}
```

`filters` maps zero-based field positions to exact typed values. Every filter
must match. String comparisons are case-sensitive and integers must fit signed
64 bits; wrong types or positions fail before issuing a native command.
Only declared output relations are accepted. Composition callers translate
exported ports through the installed `CompositionResolution`. The backend
does not enforce `ProgramInstance` public interfaces or registered-operation
admission; these remain the caller's responsibility at that lower boundary.
This addition does not alter MCP tools, shared-owner messages or native drivers.

`max_rows` must be between 1 and `MAX_QUERY_ROWS` (10,000). `max_bytes` must be
between 2 and `MAX_QUERY_BYTES` (4 MiB). `QueryPage.bytes` counts the exact
serialized JSON of `rows`, including the outer brackets, row brackets, commas
and escaped string bytes. It does not count page metadata or the cursor. An
empty complete result is two bytes. Defaults are 100 rows and 64 KiB.

`truncated` is true only when an additional matching row was observed; it always
comes with a continuation that advances past the returned rows. If the first
matching row cannot fit the requested byte limit, the request returns an error
with the required JSON page size. It never returns an empty, non-advancing
continuation in this case. Increase the limit if the row fits the hard cap.
An individual native record, including its newline, must fit
`MAX_NATIVE_RECORD_BYTES` (4 MiB). An over-limit or malformed record encountered
while selecting the page returns an error; it is never silently skipped to
present the page as complete. The transport drains the rest of the response
before returning a record/size error, preserving the healthy live owner.
Loss of framing or native I/O still disables the owner.

Cursors are opaque, serializable and bound to one live native owner, revision,
predicate and exact filter set. Page limits may change between requests.
Acknowledged mutations, program replacement, a new owner and checkpoint
restore invalidate prior cursors. Reads do not advance the revision. Cursors
are transport positions, not durable checkpoints or authorization tokens.
Native relation order is stable within an unchanged live revision; callers
must not treat it as a product ranking or rely on the same order after restore.

The pinned DDlog CLI supports `dump R_selected;` but has no cursor/limit command.
The runtime therefore scans and drains the entire selected relation for each
page, retaining only a capped native record and the bounded result page. It
decodes and compares rows until it finds the page and evidence of another
match, then drains the remaining bytes without accumulating them. It does not
dump unrelated relations, reconstruct a derived cache, evaluate rules in the
host, or call `query_typed` and truncate its complete vector. Limits bound
returned payload and host accumulation, **not** native output work, total bytes
read, or scan latency. Pagination rescans the selected relation. The native
CLI can block, and no operation timeout is introduced here.

For writers that need selected output reads rather than a complete delta dump,
`apply_without_deltas(&changes)` returns the acknowledged `version` and
`revision` using a plain native commit. Validation, set semantics, failure
handling, acknowledged input ownership and checkpoint behavior match `apply`.
Lost acknowledgement leaves inputs/revision unadvanced and disables the owner;
it does not authorize mutation replay. Candidate installation and checkpoint
restore already replay inputs with plain commits. The original `apply`,
`query`, `query_typed`, and MCP operations preserve their existing 4 MiB
aggregate transport and delta contracts.

The controlled fixture regressions in `tests/memory_runtime.rs` exercise
bounded pages and filtering, byte accounting, cursor binding, malformed and
over-limit native records, lost commit acknowledgement, and large selected and
unrelated outputs across replacement/checkpoint restore. The fixture is a
transport simulator and is not evidence of native rule evaluation.

Native acceptance is separately opt-in with an operator-configured driver:

```sh
DDLOG_RUNTIME_NATIVE_BUILD=/absolute/native-build-driver \
  cargo test --locked --test bounded_native -- --ignored --nocapture
```

It installs a real graph with 100 selected 64 KiB rows and 100 unrelated 64 KiB
rows, verifies every selected key across pages, rejects a native record above
4 MiB without losing the owner, then checks mutation, replay and checkpoint
reopen. Build logs and the checkpoint remain under the printed temporary
directory for inspection. The driver controls whether compilation is fresh or
an exact native artifact is reused; preserve that evidence separately.
