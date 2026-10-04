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
        external_publication: None,
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
            external_publication: None,
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
            external_publication: None,
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

#[test]
#[ignore = "requires DDLOG_RUNTIME_NATIVE_BUILD and its operator-configured native toolchain"]
fn managed_json_restore_recomputes_public_state_and_preserves_process_capture() {
    let root = std::env::temp_dir().join(format!(
        "world-checkpoint-native-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let driver: std::path::PathBuf = std::env::var_os("DDLOG_RUNTIME_NATIVE_BUILD")
        .expect("Configure native build driver")
        .into();
    let mut manager =
        WorldManager::new(root.join("registry"), root.join("worlds"), driver.clone()).unwrap();
    let leaf = manager
        .registry()
        .unwrap()
        .create(
            serde_json::from_value(json!({
                "rules":"reach(X,Y) :- edge(X,Y). reach(X,Z) :- reach(X,Y), edge(Y,Z).",
                "schemas":{
                    "edge":{"input":true,"fields":["int","int"]},
                    "unused":{"input":true,"fields":["string"]},
                    "reach":{"input":false,"fields":["int","int"]}},
                "interface":{"inputs":["edge","unused"],"outputs":["reach"]}
            }))
            .unwrap(),
            None,
        )
        .unwrap();
    let composed = manager.registry().unwrap().create(serde_json::from_value(json!({"composition":{
        "nodes":{"graph":{"processor_id":leaf.processor_id,"version":leaf.version}},
        "inputs":{
            "edge":{"fields":["int","int"],"targets":[{"node":"graph","relation":"edge"}]},
            "unused":{"fields":["string"],"targets":[{"node":"graph","relation":"unused"}]}},
        "bindings":[],"outputs":{"reach":{"node":"graph","relation":"reach"}}}
    })).unwrap(),None).unwrap();
    let mut evidence = Vec::new();
    for record in [leaf, composed] {
        let id = manager
            .create(world(
                "Native managed recovery",
                &serde_json::to_value(&record).unwrap(),
            ))
            .unwrap();
        let start = manager.start(&id).unwrap();
        let original_pid = start["resources"]["pid"].as_u64().unwrap() as i32;
        manager
            .execute(
                &id,
                "apply_changes",
                &json!({"changes":[
            {"op":"insert","predicate":"edge","values":[1,2]},
            {"op":"insert","predicate":"edge","values":[2,3]}]}),
            )
            .unwrap();
        let expected = manager
            .execute(&id, "query_rows", &json!({"predicate":"reach"}))
            .unwrap()["rows"]
            .clone();
        assert_eq!(expected.as_array().unwrap().len(), 3);
        let receipt = manager.checkpoint(&id).unwrap();
        manager
            .execute(
                &id,
                "apply_changes",
                &json!({"changes":[{"op":"insert","predicate":"edge","values":[3,4]}]}),
            )
            .unwrap();
        assert_eq!(
            manager
                .execute(&id, "query_rows", &json!({"predicate":"reach"}))
                .unwrap()["total"],
            6
        );
        manager.stop(&id).unwrap();
        assert_eq!(unsafe { libc::kill(original_pid, 0) }, -1);
        drop(manager);
        manager =
            WorldManager::new(root.join("registry"), root.join("worlds"), driver.clone()).unwrap();
        assert_eq!(
            manager.status(&id).unwrap()["persistence"]["checkpoints"][0]["receipt"],
            receipt
        );
        let admission = manager.restore_async(&id, &receipt).unwrap();
        assert_eq!(admission["state"], "starting");
        let restored = wait_until(&mut manager, &id, "restored native instance", |s| {
            s["state"] != "starting"
        });
        assert_eq!(restored["state"], "running", "{restored}");
        assert_eq!(restored["generation"], 2);
        assert_eq!(restored["revision"], 2);
        assert_eq!(restored["persistence"]["restored_from"], receipt);
        assert_eq!(
            restored["instance"]["source_sha256"],
            receipt["program"]["source_sha256"]
        );
        let pid = restored["resources"]["pid"].as_u64().unwrap() as i32;
        assert_ne!(pid, original_pid);
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        assert_eq!(restored["managed_processes"][0]["role"], "native");
        assert_eq!(
            manager
                .execute(&id, "query_rows", &json!({"predicate":"reach"}))
                .unwrap()["rows"],
            expected
        );
        assert_eq!(
            manager
                .execute(&id, "query_rows", &json!({"predicate":"unused"}))
                .unwrap()["rows"],
            json!([])
        );
        assert!(manager
            .execute(&id, "query_rows", &json!({"predicate":"Module0_reach"}))
            .is_err());
        manager
            .execute(
                &id,
                "apply_changes",
                &json!({"changes":[
            {"op":"delete","predicate":"edge","values":[2,3]},
            {"op":"insert","predicate":"edge","values":[2,5]}]}),
            )
            .unwrap();
        let mut after = manager
            .execute(&id, "query_rows", &json!({"predicate":"reach"}))
            .unwrap()["rows"]
            .as_array()
            .unwrap()
            .clone();
        after.sort_by_key(Value::to_string);
        assert_eq!(
            after,
            json!([[1, 2], [1, 5], [2, 5]]).as_array().unwrap().clone()
        );
        let captured = wait_until(&mut manager, &id, "restored generation capture", |s| {
            s["inspection"]["state"] == "available"
                && s["inspection"]["activity"]["totals"]["timely"]
                    .as_u64()
                    .unwrap_or(0)
                    > 0
        });
        assert!(root
            .join("worlds")
            .join(&id)
            .join("2/native-events.jsonl")
            .is_file());
        evidence.push(json!({"receipt":receipt,"restored_instance":restored["instance"],
            "rows_before":expected,"rows_after":after,"capture_state":captured["inspection"]["state"],
            "provider_calls":0}));
        manager.stop(&id).unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        let fresh = manager.start(&id).unwrap();
        assert_eq!(fresh["revision"], 1);
        assert_eq!(fresh["persistence"]["restored_from"], Value::Null);
        assert_eq!(
            manager
                .execute(&id, "query_rows", &json!({"predicate":"reach"}))
                .unwrap()["rows"],
            json!([])
        );
        manager.stop(&id).unwrap();
    }
    drop(manager);
    if let Some(path) = std::env::var_os("DDLOG_RUNTIME_NATIVE_EVIDENCE") {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&json!({"mode":"fresh native compilation","cases":evidence}))
                .unwrap(),
        )
        .unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires DDLOG_RUNTIME_NATIVE_BUILD and its operator-configured native toolchain"]
fn external_cut_freezes_real_fixed_point_restores_and_retracts() {
    use ddlog_runtime::worlds::{
        BoundaryKey, ExternalPublicationPolicy, ExternalReceipt, FrozenManifest,
    };
    use sha2::{Digest, Sha256};
    let digest = |v: &Value| format!("{:x}", Sha256::digest(serde_json::to_vec(v).unwrap()));
    let root = std::env::temp_dir().join(format!(
        "external-cut-native-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let driver =
        std::env::var_os("DDLOG_RUNTIME_NATIVE_BUILD").expect("Configure native build driver");
    let mut m =
        WorldManager::new(root.join("registry"), root.join("worlds"), driver.into()).unwrap();
    let record=m.registry().unwrap().create(serde_json::from_value(json!({
        "rules":"reach(X,Y) :- edge(X,Y). reach(X,Z) :- reach(X,Y), edge(Y,Z).",
        "schemas":{"edge":{"input":true,"fields":["int","int"]},"reach":{"input":false,"fields":["int","int"]}}
    })).unwrap(),None).unwrap();
    let mut definition = world("Native external cut", &json!(record));
    definition.external_publication = Some(ExternalPublicationPolicy {
        namespace: "archetype".into(),
        outputs: vec!["reach".into()],
        max_rows: 1000,
        max_bytes: 1024 * 1024,
    });
    let id = m.create(definition).unwrap();
    m.start(&id).unwrap();
    let apply = |m: &mut WorldManager,
                 generation: u64,
                 revision: u64,
                 key: &str,
                 parent: Option<&str>,
                 changes: Value| {
        let result=m.admit_boundary_async(serde_json::from_value(json!({"admission":{"id":id,"expected_generation":generation,
            "expected_revision":revision,"admission_key":key,"changes":changes},"binding":{"context":{"world":"ecs","run":"run-a","tick":revision},"parent_receipt_sha256":parent}})).unwrap()).unwrap();
        let key: BoundaryKey = serde_json::from_value(result["boundary"]["key"].clone()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let status = m
                .admission_status(
                    serde_json::from_value(
                        json!({"id":id,"generation":generation,"admission_key":key.admission_key}),
                    )
                    .unwrap(),
                )
                .unwrap();
            if status["state"] != "pending" {
                assert_eq!(status["state"], "frozen", "{status}");
                return (key, status);
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };
    let (key, frozen) = apply(
        &mut m,
        1,
        1,
        "first",
        None,
        json!([
        {"op":"insert","predicate":"edge","values":[1,2]}, {"op":"insert","predicate":"edge","values":[2,3]}]),
    );
    let manifest: FrozenManifest =
        serde_json::from_value(frozen["boundary"]["manifest"].clone()).unwrap();
    assert_eq!(manifest.outputs[0].rows, 3);
    let fetch = |m: &mut WorldManager, key: &BoundaryKey, sha: &str| {
        let page = m
            .read_boundary_blob(
                serde_json::from_value(
                    json!({"key":key,"blob_sha256":sha,"offset":0,"max_bytes":4*1024*1024}),
                )
                .unwrap(),
            )
            .unwrap();
        assert!(page.next_offset.is_none());
        page.bytes
    };
    let rows: Value =
        serde_json::from_slice(&fetch(&mut m, &key, &manifest.outputs[0].blob.sha256)).unwrap();
    assert!(rows.as_array().unwrap().contains(&json!([1, 3])));
    let checkpoint = fetch(&mut m, &key, &manifest.checkpoint.sha256);
    m.stop(&id).unwrap();
    let body = json!({"cut":"host-cut-1"});
    let frozen_manifest_sha256 = digest(&json!(manifest));
    let ack = ExternalReceipt {
        receipt_sha256: digest(
            &json!({"frozen_manifest_sha256":frozen_manifest_sha256,"receipt":body}),
        ),
        frozen_manifest_sha256,
        receipt: body,
    };
    m.confirm_boundary_published(key, ack.clone()).unwrap();
    drop(m);
    let driver = std::env::var_os("DDLOG_RUNTIME_NATIVE_BUILD").unwrap();
    let mut m =
        WorldManager::new(root.join("registry"), root.join("worlds"), driver.into()).unwrap();
    m.restore_boundary_async(
        serde_json::from_value(json!({"target_world_id":id,"expected_generation":1,
        "manifest":manifest,"checkpoint_bytes":checkpoint,"published":ack}))
        .unwrap(),
    )
    .unwrap();
    let live = wait_until(&mut m, &id, "bound checkpoint restore", |s| {
        s["state"] != "starting"
    });
    assert_eq!(live["state"], "running", "{live}");
    assert_eq!(
        live["persistence"]["restored_from"]["origin"]["generation"],
        1
    );
    assert!(live["instance"]["build"]["native_sha256"].is_string());
    let (_, empty) = apply(
        &mut m,
        2,
        2,
        "retract",
        Some(&ack.receipt_sha256),
        json!([
        {"op":"delete","predicate":"edge","values":[1,2]}, {"op":"delete","predicate":"edge","values":[2,3]}]),
    );
    assert_eq!(empty["boundary"]["manifest"]["outputs"][0]["rows"], 0);
    drop(m);
    eprintln!(
        "native external-cut evidence retained at {}",
        root.display()
    );
}
