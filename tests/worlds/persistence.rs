//! Managed persistence contract under a simulated transport, not native DDLog proof.
use super::*;
use sha2::{Digest, Sha256};
use std::path::Path;

fn change(manager: &mut WorldManager, id: &str, op: &str, predicate: &str, values: Value) {
    manager
        .execute(
            id,
            "apply_changes",
            &json!({"changes":[{"op":op,"predicate":predicate,"values":values}]}),
        )
        .unwrap();
}
fn rows(manager: &mut WorldManager, id: &str, predicate: &str) -> Value {
    manager
        .execute(id, "query_rows", &json!({"predicate":predicate}))
        .unwrap()["rows"]
        .clone()
}
fn receipt_dir(f: &Fixture, receipt: &Value) -> PathBuf {
    f.root
        .join("worlds")
        .join(receipt["origin"]["world_id"].as_str().unwrap())
        .join("checkpoints")
        .join(receipt["receipt_id"].as_str().unwrap())
}
fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}
fn restore(manager: &mut WorldManager, id: &str, receipt: &Value) -> Value {
    manager.restore_async(id, receipt).unwrap();
    let status = wait_until(manager, id, |s| s["state"] != "starting");
    assert_eq!(status["state"], "running", "{status}");
    status
}

#[test]
fn published_boundary_survives_restart_and_restore_is_explicit() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let mut def = echo_definition();
    def["schemas"]["unused"] = json!({"input":true,"fields":["string"]});
    def["interface"] = json!({"inputs":["source","unused"],"outputs":["echo"]});
    let record = manager
        .registry()
        .unwrap()
        .create(serde_json::from_value(def).unwrap(), None)
        .unwrap();
    let def = WorldDefinition {
        label: "checkpoint".into(),
        processor: ProcessorReference {
            processor_id: record.processor_id,
            version: record.version,
        },
        purpose: "instance".into(),
        scenarios: vec![],
    };
    let id = manager.create(def.clone()).unwrap();
    assert!(manager.checkpoint(&id).is_err());
    manager.start(&id).unwrap();
    change(
        &mut manager,
        &id,
        "insert",
        "source",
        json!([-7, "saved\n\"é"]),
    );
    let receipt = manager.checkpoint(&id).unwrap();
    assert_eq!(receipt["origin"]["revision"], 2);
    assert_eq!(receipt["program"]["lowering_version"], 2);
    assert_eq!(
        receipt["origin"]["build"]["native_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    let object = read_json(&receipt_dir(&f, &receipt).join("checkpoint.json"));
    assert_eq!(object["state"]["inputs"]["unused"], json!([]));
    change(
        &mut manager,
        &id,
        "insert",
        "source",
        json!([9, "unpublished"]),
    );
    assert!(manager.restore_async(&id, &receipt).is_err());
    let status = manager.status(&id).unwrap();
    assert_eq!(status["persistence"]["schema_version"], 1);
    assert_eq!(status["persistence"]["committed_revision"], 3);
    assert_eq!(status["persistence"]["persisted_revision"], 2);
    assert_eq!(status["persistence"]["checkpoints"][0]["receipt"], receipt);
    // Model an abrupt owner exit with its last durable running record. No
    // process is adopted when that record and receipt are reopened.
    let record_path = f.root.join("worlds").join(&id).join("world.json");
    let running_record = fs::read(&record_path).unwrap();
    drop(manager);
    fs::write(record_path, running_record).unwrap();
    let mut manager = f.manager();
    let status = manager.status(&id).unwrap();
    assert_eq!(status["state"], "interrupted");
    assert_eq!(status["persistence"]["checkpoints"][0]["receipt"], receipt);
    assert_eq!(status["resources"]["pid"], Value::Null);
    let status = restore(&mut manager, &id, &receipt);
    assert_eq!(status["generation"], 2);
    assert_eq!(status["revision"], 2);
    assert_eq!(status["persistence"]["restored_from"], receipt);
    assert_eq!(status["instance"]["processor"], json!(def.processor));
    assert_eq!(rows(&mut manager, &id, "echo"), json!([[-7, "saved\n\"é"]]));
    assert_eq!(rows(&mut manager, &id, "unused"), json!([]));
    change(
        &mut manager,
        &id,
        "delete",
        "source",
        json!([-7, "saved\n\"é"]),
    );
    change(&mut manager, &id, "insert", "source", json!([8, "new"]));
    assert_eq!(rows(&mut manager, &id, "echo"), json!([[8, "new"]]));
    // A created world with the same pin can explicitly use the original receipt.
    let target = manager.create(def).unwrap();
    assert_eq!(restore(&mut manager, &target, &receipt)["generation"], 1);
    assert_eq!(
        rows(&mut manager, &target, "source"),
        json!([[-7, "saved\n\"é"]])
    );
    manager.stop(&id).unwrap();
    let fresh = manager.start(&id).unwrap();
    assert_eq!(fresh["generation"], 3);
    assert_eq!(fresh["revision"], 1);
    assert_eq!(fresh["persistence"]["restored_from"], Value::Null);
    assert_eq!(fresh["persistence"]["persisted_revision"], Value::Null);
    assert_eq!(rows(&mut manager, &id, "source"), json!([]));
    manager.stop(&id).unwrap();
    manager.stop(&target).unwrap();
    drop(manager);
    let mut manager = f.manager();
    let status = manager.status(&id).unwrap();
    assert!(status["history"]
        .as_array()
        .unwrap()
        .iter()
        .any(|h| h["restored_from"] == receipt));
    assert_eq!(status["persistence"]["checkpoints"][0]["receipt"], receipt);
}

#[test]
fn receipt_tampering_missing_objects_and_paths_fail_closed() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    manager.start(&id).unwrap();
    let receipt = manager.checkpoint(&id).unwrap();
    manager.stop(&id).unwrap();
    for (pointer, value) in [
        ("/receipt_id", json!("../../world.json")),
        ("/receipt_id", json!("/tmp/checkpoint.json")),
        ("/origin/world_id", json!("../outside")),
        ("/origin/revision", json!(99)),
        ("/origin/build/native_sha256", json!("tampered")),
        ("/program/lowering_version", json!(1)),
        ("/program/public_relations/0/physical", json!("private")),
        (
            "/program/dependencies",
            json!({"fake":{"processor_id":"fake","version":"fake"}}),
        ),
        ("/checkpoint_sha256", json!("0".repeat(64))),
    ] {
        let mut changed = receipt.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert!(manager.restore_async(&id, &changed).is_err(), "{pointer}");
        assert_eq!(manager.status(&id).unwrap()["generation"], 1);
    }
    assert!(manager
        .restore_async(&id, &json!("/tmp/checkpoint.json"))
        .is_err());
    let dir = receipt_dir(&f, &receipt);
    let path = dir.join("checkpoint.json");
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, b"corrupt").unwrap();
    assert!(manager.restore_async(&id, &receipt).is_err());
    fs::remove_file(&path).unwrap();
    assert_eq!(
        manager.status(&id).unwrap()["persistence"]["checkpoints"][0]["availability"],
        "missing_or_invalid"
    );
    assert!(manager.restore_async(&id, &receipt).is_err());
    let outside = f.root.join("outside.json");
    fs::write(&outside, &bytes).unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    assert!(manager.restore_async(&id, &receipt).is_err());
    fs::remove_file(&path).unwrap();
    fs::write(&path, bytes).unwrap();
    let other = definition(&manager);
    let other = manager.create(other).unwrap();
    assert!(manager
        .restore_async(&other, &receipt)
        .unwrap_err()
        .contains("pin"));
    let mut manifest = read_json(&dir.join("publication.json"));
    manifest["status"] = json!("staged");
    write_json(&dir.join("publication.json"), &manifest);
    assert!(manager.restore_async(&id, &receipt).is_err());
    drop(manager);
    let mut manager = f.manager();
    assert_eq!(
        manager.status(&id).unwrap()["persistence"]["checkpoints"][0]["status"],
        "staged"
    );
    assert!(manager.restore_async(&id, &receipt).is_err());
}

#[test]
fn forged_lowered_source_is_rejected_before_compilation_even_with_valid_digest() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    manager.start(&id).unwrap();
    let mut receipt = manager.checkpoint(&id).unwrap();
    manager.stop(&id).unwrap();
    let dir = receipt_dir(&f, &receipt);
    let path = dir.join("checkpoint.json");
    let mut checkpoint = read_json(&path);
    let source = checkpoint["state"]["source"].as_str().unwrap().to_owned();
    checkpoint["state"]["source"] = json!(format!("{source}\n// changed source\n"));
    checkpoint["sha256"] = json!(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&checkpoint["state"]).unwrap())
    ));
    receipt["checkpoint_sha256"] = checkpoint["sha256"].clone();
    write_json(&path, &checkpoint);
    let mut publication = read_json(&dir.join("publication.json"));
    publication["receipt"] = receipt.clone();
    write_json(&dir.join("publication.json"), &publication);
    manager.restore_async(&id, &receipt).unwrap();
    let status = wait_until(&mut manager, &id, |s| s["state"] != "starting");
    assert_eq!(status["state"], "failed");
    assert!(status["error"].as_str().unwrap().contains("pinned"));
    assert!(!f.root.join("worlds").join(id).join("2/build-1").exists());
    assert_eq!(status["persistence"]["restored_from"], Value::Null);
}

#[test]
fn composition_restore_retains_public_mapping_exact_dependencies_and_admission() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let mut leaf = echo_definition();
    leaf["interface"] = json!({"inputs":["source"],"outputs":["echo"]});
    let leaf = manager
        .registry()
        .unwrap()
        .create(serde_json::from_value(leaf).unwrap(), None)
        .unwrap();
    let leaf = ProcessorReference {
        processor_id: leaf.processor_id,
        version: leaf.version,
    };
    let composition: ProcessorDefinition = serde_json::from_value(json!({"composition":{
        "nodes":{"child":leaf},"inputs":{"in":{"fields":["int","string"],"targets":[{"node":"child","relation":"source"}]}},
        "bindings":[],"outputs":{"out":{"node":"child","relation":"echo"}}}})).unwrap();
    let record = manager
        .registry()
        .unwrap()
        .create(composition, None)
        .unwrap();
    let id = manager
        .create(WorldDefinition {
            label: "composition".into(),
            processor: ProcessorReference {
                processor_id: record.processor_id,
                version: record.version,
            },
            purpose: "instance".into(),
            scenarios: vec![],
        })
        .unwrap();
    manager.start(&id).unwrap();
    change(&mut manager, &id, "insert", "in", json!([1, "retained"]));
    let receipt = manager.checkpoint(&id).unwrap();
    assert!(!receipt["program"]["dependencies"]
        .as_object()
        .unwrap()
        .is_empty());
    manager.stop(&id).unwrap();
    // Current-version changes do not replace the dependency's exact version.
    let mut next = echo_definition();
    next["interface"] = json!({"inputs":["source"],"outputs":["echo"]});
    next["rules"] = json!("echo(N,S) :- source(N,S). echo(N,S) :- source(N,S), N > 0.");
    manager
        .registry()
        .unwrap()
        .publish(
            &leaf.processor_id,
            serde_json::from_value(next).unwrap(),
            &leaf.version,
            None,
        )
        .unwrap();
    let status = restore(&mut manager, &id, &receipt);
    assert_eq!(status["instance"]["composition"]["lowering_version"], 2);
    assert_eq!(rows(&mut manager, &id, "in"), json!([[1, "retained"]]));
    assert!(manager
        .execute(&id, "query_rows", &json!({"predicate":"Module0_source"}))
        .is_err());
    assert!(manager
        .execute(
            &id,
            "apply_changes",
            &json!({"changes":[{"op":"insert","predicate":"Input_in","values":[2,"private"]}]})
        )
        .is_err());
    change(&mut manager, &id, "delete", "in", json!([1, "retained"]));
    assert_eq!(rows(&mut manager, &id, "in"), json!([]));
    change(&mut manager, &id, "insert", "in", json!([2, "new"]));
    assert_eq!(rows(&mut manager, &id, "in"), json!([[2, "new"]]));
    manager.stop(&id).unwrap();
    let path = f
        .root
        .join("registry")
        .join(&leaf.processor_id)
        .join("versions")
        .join(format!(
            "{}.json",
            leaf.version.strip_prefix("sha256:").unwrap()
        ));
    fs::remove_file(path).unwrap();
    assert!(manager.restore_async(&id, &receipt).is_err());
    assert_eq!(manager.status(&id).unwrap()["generation"], 2);
}

#[test]
fn failed_publication_preserves_prior_receipt_and_live_state() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    manager.start(&id).unwrap();
    let receipt = manager.checkpoint(&id).unwrap();
    change(
        &mut manager,
        &id,
        "insert",
        "source",
        json!([1, "unpublished"]),
    );
    let checkpoints = f.root.join("worlds").join(&id).join("checkpoints");
    let retained = f.root.join("retained");
    fs::rename(&checkpoints, &retained).unwrap();
    fs::write(&checkpoints, "blocked").unwrap();
    assert!(manager.checkpoint(&id).is_err());
    assert_eq!(
        rows(&mut manager, &id, "source"),
        json!([[1, "unpublished"]])
    );
    fs::remove_file(&checkpoints).unwrap();
    fs::rename(retained, &checkpoints).unwrap();
    assert_eq!(
        manager.status(&id).unwrap()["persistence"]["checkpoints"][0]["receipt"],
        receipt
    );
    // A failure after receipt publication must not lose the durable receipt.
    let temp = f.root.join("worlds").join(&id).join("world.json.tmp");
    fs::create_dir(&temp).unwrap();
    assert!(manager.checkpoint(&id).is_err());
    fs::remove_dir(temp).unwrap();
    drop(manager);
    let mut manager = f.manager();
    let status = manager.status(&id).unwrap();
    let checkpoints = status["persistence"]["checkpoints"].as_array().unwrap();
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(checkpoints[1]["status"], "published");
    restore(&mut manager, &id, &checkpoints[1]["receipt"]);
    assert_eq!(
        rows(&mut manager, &id, "source"),
        json!([[1, "unpublished"]])
    );
}

#[test]
fn restore_build_and_replay_failures_are_retryable_explicitly() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    manager.start(&id).unwrap();
    let receipt = manager.checkpoint(&id).unwrap();
    manager.stop(&id).unwrap();
    for flag in ["reject_build", "fail_replay"] {
        fs::write(f.root.join(flag), "").unwrap();
        manager.restore_async(&id, &receipt).unwrap();
        let failed = wait_until(&mut manager, &id, |s| s["state"] != "starting");
        assert_eq!(failed["state"], "failed");
        assert_eq!(failed["persistence"]["restored_from"], Value::Null);
        assert_eq!(failed["managed_processes"], json!([]));
        fs::remove_file(f.root.join(flag)).unwrap();
    }
    assert_eq!(restore(&mut manager, &id, &receipt)["generation"], 4);
}

#[test]
fn restore_retains_hosted_compiler_cancellation_and_observer_options() {
    let f = Fixture::new();
    let driver = f.root.join("build.py");
    let mut script = fs::read_to_string(&driver).unwrap();
    script = script.replace("import sys", "import sys, os, time, subprocess");
    script = script.replace("if (root/'reject_build')", "if (root/'hold_build').exists():\n child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(3600)'])\n (root/'compiler.pending').write_text(__import__('json').dumps({'pid':os.getpid(),'child':child.pid,'pgid':os.getpgrp()}))\n os.replace(root/'compiler.pending', root/'compiler.json')\n while True: time.sleep(0.01)\nif (root/'reject_build')");
    let injected = "(control/'observer.json').write_text(json.dumps({'detail':os.environ.get('DDLOG_OBSERVER_DETAIL'),'rotate':os.environ.get('DDLOG_OBSERVER_ROTATE'),'file':os.environ.get('DDLOG_OBSERVER_FILE'),'pid':os.getpid(),'pgid':os.getpgrp()}))\nfacts, staged = {}, {}";
    script = script.replace("Path(sys.argv[2]).write_text(source)", &format!("source=source.replace('facts, staged = {{}}, {{}}', {})\nPath(sys.argv[2]).write_text(source)", json!(injected)));
    fs::write(&driver, script).unwrap();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    manager.start(&id).unwrap();
    let receipt = manager.checkpoint(&id).unwrap();
    manager.stop(&id).unwrap();
    fs::write(f.root.join("hold_build"), "").unwrap();
    let beginning = std::time::Instant::now();
    manager.restore_async(&id, &receipt).unwrap();
    assert!(beginning.elapsed() < std::time::Duration::from_secs(2));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !f.root.join("compiler.json").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let compiler = read_json(&f.root.join("compiler.json"));
    assert_eq!(compiler["pid"], compiler["pgid"]);
    assert!(manager.restore_async(&id, &receipt).is_err());
    manager.stop(&id).unwrap();
    let stopped = wait_until(&mut manager, &id, |s| s["state"] == "stopped");
    assert_eq!(stopped["managed_processes"], json!([]));
    assert_eq!(stopped["persistence"]["restored_from"], Value::Null);
    assert_eq!(
        unsafe { libc::kill(compiler["pid"].as_i64().unwrap() as i32, 0) },
        -1
    );
    let child = compiler["child"].as_i64().unwrap() as i32;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while unsafe { libc::kill(child, 0) } == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "compiler descendant survived cancellation"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    fs::remove_file(f.root.join("hold_build")).unwrap();
    let status = restore(&mut manager, &id, &receipt);
    let observed = read_json(&f.root.join("observer.json"));
    assert_eq!(observed["detail"], "full");
    assert_eq!(observed["rotate"], "1");
    assert_eq!(observed["pid"], observed["pgid"]);
    assert_eq!(observed["pid"], status["resources"]["pid"]);
    assert_eq!(
        observed["file"],
        json!(f
            .root
            .join("worlds")
            .join(&id)
            .join("3/native-events.jsonl"))
    );
    assert_eq!(status["managed_processes"][0]["role"], "native");
    let pid = observed["pid"].as_i64().unwrap() as i32;
    manager.shutdown_handle().stop_all();
    wait_until(&mut manager, &id, |s| s["state"] == "failed");
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
}

#[test]
fn old_world_records_default_to_empty_persistence_and_unsupported_imports_fail_explicitly() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    drop(manager);
    let path = f.root.join("worlds").join(&id).join("world.json");
    let mut old = read_json(&path);
    old.as_object_mut().unwrap().remove("persistence");
    write_json(&path, &old);
    let mut manager = f.manager();
    let status = manager.status(&id).unwrap();
    assert_eq!(status["state"], "created");
    assert_eq!(status["persistence"]["checkpoints"], json!([]));
    let star = manager.registry().unwrap().create(serde_json::from_value(json!({
        "rules":"", "schemas":{
            "vertices":{"input":true,"fields":["int"]},
            "edges":{"input":true,"fields":["int","int"]},
            "labels":{"input":false,"fields":["int","int"]}},
        "operators":[{"type":"large_small_star","vertices":"vertices","edges":"edges","output":"labels"}],
        "interface":{"inputs":["vertices","edges"],"outputs":["labels"]}
    })).unwrap(),None).unwrap();
    let id = manager
        .create(WorldDefinition {
            label: "unsupported checkpoint".into(),
            processor: ProcessorReference {
                processor_id: star.processor_id,
                version: star.version,
            },
            purpose: "instance".into(),
            scenarios: vec![],
        })
        .unwrap();
    manager.start(&id).unwrap();
    assert!(manager
        .checkpoint(&id)
        .unwrap_err()
        .contains("imported native operators"));
    let status = manager.status(&id).unwrap();
    assert_eq!(status["state"], "running");
    assert_eq!(status["persistence"]["status"], "error");
    assert_eq!(status["persistence"]["checkpoints"], json!([]));
}
