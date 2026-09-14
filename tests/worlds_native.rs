#![cfg(unix)]
//! Real native acceptance, explicitly configured by the operator.
use ddlog_runtime::{
    registry::{ProcessorDefinition, ProcessorReference},
    worlds::{Scenario, TestRequest, WorldDefinition, WorldManager},
};
use serde_json::{json, Value};
fn wait_until(
    manager: &mut WorldManager,
    id: &str,
    what: &str,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let status = manager.status(id).unwrap();
        if done(&status) {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}: {status}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
fn world(label: &str, record: &Value) -> WorldDefinition {
    WorldDefinition {
        label: label.into(),
        processor: ProcessorReference {
            processor_id: record["processor_id"].as_str().unwrap().into(),
            version: record["version"].as_str().unwrap().into(),
        },
        purpose: "instance".into(),
        scenarios: vec![],
    }
}
#[test]
#[ignore = "requires DDLOG_RUNTIME_NATIVE_BUILD and its operator-configured native toolchain"]
fn registered_worlds_expose_native_graph_and_metadata_then_stop() {
    let root = std::env::temp_dir().join(format!("world-native-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let driver =
        std::env::var_os("DDLOG_RUNTIME_NATIVE_BUILD").expect("Configure native build driver");
    let mut manager =
        WorldManager::new(root.join("registry"), root.join("worlds"), driver.into()).unwrap();
    let definition:ProcessorDefinition=serde_json::from_value(json!({"rules":"reach(X,Y) :- edge(X,Y). reach(X,Z) :- reach(X,Y), edge(Y,Z).","schemas":{"edge":{"input":true,"fields":["int","int"]},"reach":{"input":false,"fields":["int","int"]}}})).unwrap();
    let first = manager
        .registry()
        .unwrap()
        .create(definition.clone(), None)
        .unwrap();
    let a = manager
        .create(WorldDefinition {
            label: "Native reachability".into(),
            processor: ProcessorReference {
                processor_id: first.processor_id.clone(),
                version: first.version.clone(),
            },
            purpose: "instance".into(),
            scenarios: vec![],
        })
        .unwrap();
    let started = manager.start(&a).unwrap();
    assert_eq!(started["state"], "running");
    // The capture is tailed asynchronously; `running` does not imply ingested.
    let status = wait_until(&mut manager, &a, "native topology", |s| {
        s["inspection"]["state"] == "available"
    });
    assert!(
        status["inspection"]["graph"]["nodes"]
            .as_array()
            .unwrap()
            .len()
            > 1
    );
    manager.execute(&a,"apply_changes",&json!({"changes":[{"op":"insert","predicate":"edge","values":[1,2]},{"op":"insert","predicate":"edge","values":[2,3]}]})).unwrap();
    let rows = manager
        .execute(&a, "lemmalog_query", &json!({"predicate":"reach"}))
        .unwrap();
    assert!(rows["rows"].as_str().unwrap().contains(".f0 = 1, .f1 = 3"));
    let relations = manager.execute(&a, "relations", &json!({})).unwrap();
    assert_eq!(relations["revision"], 2);
    assert_eq!(
        relations["relations"],
        json!([{"name":"edge","input":true,"fields":["int","int"],"count":2},{"name":"reach","input":false,"fields":["int","int"],"count":3}])
    );
    let page = manager
        .execute(&a, "query_rows", &json!({"predicate":"reach","max_rows":2}))
        .unwrap();
    assert_eq!(page["total"], 3);
    assert_eq!(page["complete"], false);
    assert_eq!(page["rows"].as_array().unwrap().len(), 2);
    let rest = manager
        .execute(
            &a,
            "query_rows",
            &json!({"predicate":"reach","max_rows":2,"continuation":page["continuation"]}),
        )
        .unwrap();
    assert_eq!(rest["complete"], true);
    let mut all: Vec<serde_json::Value> = page["rows"]
        .as_array()
        .unwrap()
        .iter()
        .chain(rest["rows"].as_array().unwrap())
        .cloned()
        .collect();
    all.sort_by_key(|row| row.to_string());
    assert_eq!(
        all,
        json!([[1, 2], [1, 3], [2, 3]]).as_array().unwrap().clone()
    );
    let source = manager.execute(&a, "program_source", &json!({})).unwrap();
    assert!(source["source"].as_str().unwrap().contains("R_reach"));
    assert_eq!(
        source["source_sha256"],
        manager.status(&a).unwrap()["instance"]["source_sha256"]
    );
    // Full-detail capture: the transaction above scheduled operators and sent
    // messages; the tailer ingests them without making the world unavailable.
    let active = wait_until(&mut manager, &a, "native activity", |s| {
        let activity = &s["inspection"]["activity"];
        activity["complete"] == json!(true)
            && activity["nodes"]
                .as_object()
                .unwrap()
                .values()
                .any(|n| n["schedule_count"].as_u64().unwrap_or(0) > 0)
            && activity["channels"]
                .as_object()
                .unwrap()
                .values()
                .any(|c| c["records"].as_u64().unwrap_or(0) > 0)
    });
    let activity = &active["inspection"]["activity"];
    assert_eq!(active["inspection"]["state"], "available");
    assert!(activity["totals"]["timely"].as_u64().unwrap() > 0);
    assert!(activity["totals"]["progress"].as_u64().unwrap() > 0);
    assert!(activity["totals"]["differential"].as_u64().unwrap() > 0);
    assert_eq!(activity["rotations"], 0);
    assert_eq!(activity["truncated_at_bytes"], Value::Null);
    assert!(activity["last_event_ns"].as_u64().unwrap() > 0);
    assert_eq!(activity["totals"]["unresolved_channels"], 0);
    // The compiled topology was retained for the definition version.
    let first_pin = (first.processor_id.clone(), first.version.clone());
    let capture = manager.capture_get(&first_pin.0, &first_pin.1).unwrap();
    assert_eq!(capture["world_id"], a);
    assert_eq!(capture["generation"], 1);
    assert_eq!(
        capture["graph"]["nodes"],
        active["inspection"]["graph"]["nodes"]
    );
    assert_eq!(capture["unresolved_channels"], 0);
    // Asynchronous scenario test against the same definition version.
    let scenarios: Vec<Scenario> = serde_json::from_value(json!([
        {"name":"path","description":"","changes":[
            {"op":"insert","predicate":"edge","values":[1,2]},
            {"op":"insert","predicate":"edge","values":[2,3]}],
         "expect":{"reach":[[1,2],[1,3],[2,3]]}},
        {"name":"cut","description":"","changes":[
            {"op":"delete","predicate":"edge","values":[2,3]}],
         "expect":{"reach":[[1,2]]}}
    ]))
    .unwrap();
    manager
        .scenarios_set(&first_pin.0, &first_pin.1, scenarios)
        .unwrap();
    let test = manager
        .test(TestRequest {
            processor_id: first_pin.0.clone(),
            version: first_pin.1.clone(),
            scenarios: None,
            keep_world: false,
        })
        .unwrap();
    assert_eq!(test["state"], "starting");
    assert_eq!(test["test"]["phase"], "building");
    let test_id = test["id"].as_str().unwrap().to_string();
    let finished = wait_until(&mut manager, &test_id, "test completion", |s| {
        s["state"] != "starting"
    });
    assert_eq!(finished["state"], "stopped", "{finished}");
    assert_eq!(finished["test"]["phase"], "done");
    assert_eq!(finished["test"]["passed"], true, "{}", finished["test"]);
    assert_eq!(finished["test"]["results"][0]["revision"], 2);
    assert_eq!(finished["test"]["results"][1]["revision"], 3);
    assert_eq!(finished["resources"]["pid"], Value::Null);
    let edge = &status["inspection"]["graph"]["edges"][0];
    let mut with_metadata = serde_json::to_value(definition).unwrap();
    with_metadata["inspection"] = json!({"schema_version":1,"authoredGroups":[{"id":"source-boundary","name":"Observed source boundary","member_key":"source","provenance":{"repository":"native-acceptance","revision":"fixture","source":null},"memberIds":[edge["source"]],"ports":[{"id":"output","name":"output","direction":"output","data_type":"native_stream","native_ports":[{"operator_id":edge["source"],"index":edge["source_port"]}]}]}]});
    let second = manager
        .registry()
        .unwrap()
        .create(serde_json::from_value(with_metadata).unwrap(), None)
        .unwrap();
    let b = manager
        .create(WorldDefinition {
            label: "Native mapped world".into(),
            processor: ProcessorReference {
                processor_id: second.processor_id,
                version: second.version,
            },
            purpose: "instance".into(),
            scenarios: vec![],
        })
        .unwrap();
    let mapped = manager.start(&b).unwrap();
    assert_ne!(mapped["resources"]["pid"], status["resources"]["pid"]);
    let mapped = wait_until(&mut manager, &b, "mapped topology", |s| {
        s["inspection"]["state"] == "available"
    });
    assert_eq!(
        mapped["inspection"]["mapping_error"],
        serde_json::Value::Null
    );
    assert_eq!(
        mapped["inspection"]["metadata"]["authoredGroups"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    manager.stop(&a).unwrap();
    assert_eq!(manager.status(&b).unwrap()["state"], "running");
    manager.stop(&b).unwrap();
    // Star program: build-time phase regions resolve match-based authored groups.
    let star = manager
        .register(ddlog_runtime::worlds::RegisterRequest {
            name: "Connected components".into(),
            description: String::new(),
            library_id: None,
            definition: json!({
                "rules":"",
                "schemas":{
                    "vertices":{"input":true,"fields":["int"]},
                    "edges":{"input":true,"fields":["int","int"]},
                    "labels":{"input":false,"fields":["int","int"]}
                },
                "operators":[{"type":"large_small_star","vertices":"vertices","edges":"edges","output":"labels"}],
                "interface":{"inputs":["vertices","edges"],"outputs":["labels"]},
                "inspection":{"schema_version":1,"authoredGroups":[
                    {"id":"large-star","name":"Large star","member_key":"large_small_star/large","provenance":{"repository":"native-acceptance","revision":"fixture","source":null},"member_match":{"scope_name":"large-star"}},
                    {"id":"small-star","name":"Small star","member_key":"large_small_star/small","provenance":{"repository":"native-acceptance","revision":"fixture","source":null},"member_match":{"scope_name":"small-star"}},
                    {"id":"minimum-label","name":"Minimum label","member_key":"large_small_star/minimum","provenance":{"repository":"native-acceptance","revision":"fixture","source":null},"member_match":{"scope_name":"minimum-label"}}
                ]}
            }),
            git_provenance: None,
        })
        .unwrap();
    let c = manager
        .create(world("Native connected components", &star))
        .unwrap();
    let started = manager.start(&c).unwrap();
    assert_eq!(started["state"], "running");
    let marker = root
        .join("worlds")
        .join(&c)
        .join("1")
        .join("build-1")
        .join("program_ddlog")
        .join("observer-install.json");
    let marker: Value = serde_json::from_slice(&std::fs::read(marker).unwrap()).unwrap();
    assert_eq!(marker["observer_hook"], true);
    assert_eq!(
        marker["star_phases"], true,
        "star phases installed at build time"
    );
    manager
        .execute(
            &c,
            "apply_changes",
            &json!({"changes":[
        {"op":"insert","predicate":"vertices","values":[1]},
        {"op":"insert","predicate":"vertices","values":[2]},
        {"op":"insert","predicate":"vertices","values":[3]},
        {"op":"insert","predicate":"edges","values":[1,2]}]}),
        )
        .unwrap();
    let scoped = wait_until(&mut manager, &c, "star topology", |s| {
        s["inspection"]["state"] == "available"
            && s["inspection"]["unresolved_channels"] == 0
            && s["inspection"]["activity"]["complete"] == json!(true)
    });
    assert_eq!(
        scoped["inspection"]["mapping_error"],
        Value::Null,
        "{}",
        scoped["inspection"]["mapping_error"]
    );
    let nodes = scoped["inspection"]["graph"]["nodes"].as_array().unwrap();
    for phase in ["large-star", "small-star", "minimum-label"] {
        assert!(
            nodes.iter().any(|n| n["name"] == phase),
            "native scope {phase} present"
        );
    }
    let groups = scoped["inspection"]["metadata"]["authoredGroups"]
        .as_array()
        .unwrap();
    assert_eq!(groups.len(), 3);
    for group in groups {
        let members = group["memberIds"].as_array().unwrap();
        assert!(members.len() > 1, "{} resolved to {members:?}", group["id"]);
    }
    let labels = manager
        .execute(&c, "query_rows", &json!({"predicate":"labels"}))
        .unwrap();
    let mut rows: Vec<Value> = labels["rows"].as_array().unwrap().clone();
    rows.sort_by_key(|row| row.to_string());
    assert_eq!(
        rows,
        json!([[1, 1], [2, 1], [3, 3]]).as_array().unwrap().clone()
    );
    let capture = manager
        .capture_get(
            star["processor_id"].as_str().unwrap(),
            star["version"].as_str().unwrap(),
        )
        .unwrap();
    assert_eq!(
        capture["metadata"]["authoredGroups"][0]["memberIds"],
        groups[0]["memberIds"]
    );
    manager.stop(&c).unwrap();
    drop(manager);
    let mut recovered = WorldManager::new(
        root.join("registry"),
        root.join("worlds"),
        std::env::var_os("DDLOG_RUNTIME_NATIVE_BUILD")
            .unwrap()
            .into(),
    )
    .unwrap();
    assert_eq!(recovered.status(&a).unwrap()["state"], "stopped");
    drop(recovered);
    std::fs::remove_dir_all(root).unwrap();
}
