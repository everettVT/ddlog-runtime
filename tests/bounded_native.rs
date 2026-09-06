#![cfg(unix)]
//! Explicit native acceptance, never run with the transport simulator.
use ddlog_runtime::{Backend, BoundedQuery, MAX_NATIVE_RECORD_BYTES, MAX_QUERY_BYTES};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[test]
#[ignore = "requires DDLOG_RUNTIME_NATIVE_BUILD pointing to an operator-configured native driver"]
fn native_large_outputs_support_bounded_reads_plain_commits_and_reopen() {
    let driver = PathBuf::from(std::env::var("DDLOG_RUNTIME_NATIVE_BUILD").unwrap());
    let root = std::env::temp_dir().join(format!("ddlog-bounded-native-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let rules = "echo(N,S) :- source(N,S). unrelated(S) :- unused(S).";
    let schemas = json!({
        "source":{"input":true,"fields":["int","string"]},
        "unused":{"input":true,"fields":["string"]},
        "echo":{"input":false,"fields":["int","string"]},
        "unrelated":{"input":false,"fields":["string"]}
    });
    let mut live = Backend::new(root.join("live"), driver.clone());
    live.install(rules, schemas.clone()).unwrap();
    let payload = "x".repeat(65536);
    let changes: Vec<_> = (0..100)
        .flat_map(|index| {
            [
                json!({"op":"insert","predicate":"source","values":[index,payload]}),
                json!({"op":"insert","predicate":"unused","values":[format!("{index}:{payload}")]}),
            ]
        })
        .collect();
    live.apply_without_deltas(&json!(changes)).unwrap();
    let selected = BoundedQuery {
        filters: BTreeMap::from([(0, json!(50)), (1, json!(payload))]),
        max_rows: 1,
        max_bytes: 128 * 1024,
        continuation: None,
    };
    let exact = live.query_typed_bounded("echo", &selected).unwrap();
    assert_eq!(exact.rows, vec![vec![json!(50), json!(payload)]]);
    assert!(!exact.truncated);
    let mut query = BoundedQuery {
        max_rows: 31,
        max_bytes: 2 * 1024 * 1024,
        ..Default::default()
    };
    let mut keys = Vec::new();
    loop {
        let page = live.query_typed_bounded("echo", &query).unwrap();
        assert!(page.rows.len() <= query.max_rows);
        assert!(page.bytes <= query.max_bytes);
        assert_eq!(page.bytes, serde_json::to_vec(&page.rows).unwrap().len());
        keys.extend(page.rows.iter().map(|row| row[0].as_i64().unwrap()));
        assert_eq!(page.truncated, page.continuation.is_some());
        query.continuation = page.continuation;
        if query.continuation.is_none() {
            break;
        }
    }
    assert_eq!(keys.len(), 100);
    let unique = keys
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        unique.len(),
        keys.len(),
        "continuations must not repeat rows"
    );
    keys.sort_unstable();
    assert_eq!(keys, (0..100).collect::<Vec<_>>());
    // Even one record beyond the transport cap is drained without losing the
    // owner; the following mutation must still reach the same native graph.
    let enormous = "z".repeat(MAX_NATIVE_RECORD_BYTES + 1);
    live.apply_without_deltas(&json!([
        {"op":"insert","predicate":"source","values":[-1,enormous]}
    ]))
    .unwrap();
    let error = live
        .query_typed_bounded(
            "echo",
            &BoundedQuery {
                max_bytes: MAX_QUERY_BYTES,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(error.contains("record exceeded"), "{error}");
    assert_eq!(live.health(), "ready");
    live.apply_without_deltas(&json!([
        {"op":"delete","predicate":"source","values":[-1,enormous]},
        {"op":"insert","predicate":"source","values":[-2,"a\n\"b\"\\é🚀"]}
    ]))
    .unwrap();
    let escaped = BoundedQuery {
        filters: BTreeMap::from([(1, json!("a\n\"b\"\\é🚀"))]),
        ..Default::default()
    };
    assert_eq!(
        live.query_typed_bounded("echo", &escaped).unwrap().rows,
        vec![vec![json!(-2), json!("a\n\"b\"\\é🚀")]]
    );
    live.install(rules, schemas).unwrap();
    let checkpoint = root.join("checkpoint.json");
    live.save_checkpoint(&checkpoint, json!({"native":"bounded"}))
        .unwrap();
    let revision = live.revision();
    drop(live);
    let mut reopened = Backend::new(root.join("reopened"), driver);
    assert_eq!(
        reopened.restore_checkpoint(&checkpoint).unwrap(),
        json!({"native":"bounded"})
    );
    assert_eq!(reopened.revision(), revision);
    assert_eq!(
        reopened
            .query_typed_bounded("echo", &selected)
            .unwrap()
            .rows,
        exact.rows
    );
    assert_eq!(
        reopened.query_typed_bounded("echo", &escaped).unwrap().rows,
        vec![vec![json!(-2), json!("a\n\"b\"\\é🚀")]]
    );
    assert_eq!(reopened.health(), "ready");
    println!("Native bounded acceptance artifacts: {}", root.display());
    println!("Verified 100 x 64KiB selected rows, 100 x 64KiB unrelated rows, exact filters, paginated completeness, oversized record recovery, candidate replay, checkpoint reopen");
}
