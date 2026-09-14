#![cfg(unix)]
//! Simulated transport acceptance: these tests do not claim native DDLog evaluation.
use ddlog_runtime::registry::{ProcessorDefinition, ProcessorReference};
use ddlog_runtime::worlds::{
    ImportRequest, InventoryQuery, RegisterRequest, Scenario, TestRequest, WorldDefinition,
    WorldManager,
};
use serde_json::{json, Value};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "world-test-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let template =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/memory_fake_runtime.py");
        fs::write(root.join("build.py"),format!("#!/usr/bin/env python3\nimport sys\nfrom pathlib import Path\nroot=Path({})\nif (root/'reject_build').exists(): sys.exit(91)\nsource=Path({}).read_text().replace('__CONTROL__',{})\nPath(sys.argv[2]).write_text(source)\nPath(sys.argv[2]).chmod(0o700)\n",json!(root),json!(template),json!(serde_json::to_string(&root).unwrap()))).unwrap();
        fs::set_permissions(root.join("build.py"), fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }
    fn manager(&self) -> WorldManager {
        WorldManager::new(
            self.root.join("registry"),
            self.root.join("worlds"),
            self.root.join("build.py"),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn definition(manager: &WorldManager) -> WorldDefinition {
    let definition:ProcessorDefinition=serde_json::from_value(json!({"rules":"echo(N,S) :- source(N,S).","schemas":{"source":{"input":true,"fields":["int","string"]},"echo":{"input":false,"fields":["int","string"]}}})).unwrap();
    let record = manager
        .registry()
        .unwrap()
        .create(definition, None)
        .unwrap();
    WorldDefinition {
        label: "Test world".into(),
        processor: ProcessorReference {
            processor_id: record.processor_id,
            version: record.version,
        },
        purpose: "instance".into(),
        scenarios: vec![],
    }
}
fn echo_definition() -> Value {
    json!({"rules":"echo(N,S) :- source(N,S).","schemas":{"source":{"input":true,"fields":["int","string"]},"echo":{"input":false,"fields":["int","string"]}}})
}
/// Register through the control-plane path so the definition carries a name.
fn named(manager: &WorldManager, name: &str) -> WorldDefinition {
    let record = manager
        .register(RegisterRequest {
            name: name.into(),
            description: String::new(),
            library_id: None,
            definition: echo_definition(),
            git_provenance: None,
        })
        .unwrap();
    WorldDefinition {
        label: name.into(),
        processor: ProcessorReference {
            processor_id: record["processor_id"].as_str().unwrap().into(),
            version: record["version"].as_str().unwrap().into(),
        },
        purpose: "instance".into(),
        scenarios: vec![],
    }
}
fn inventory(manager: &mut WorldManager) -> Value {
    manager.inventory(&InventoryQuery::default()).unwrap()
}
fn wait_until(manager: &mut WorldManager, id: &str, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let status = manager.status(id).unwrap();
        if done(&status) {
            return status;
        }
        assert!(std::time::Instant::now() < deadline, "timed out: {status}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
fn fixture_registry() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/upstream/registry")
}
fn scenario(name: &str, values: Vec<Value>, expect: Vec<Vec<Value>>) -> Scenario {
    serde_json::from_value(json!({"name":name,"description":"","changes":[{"op":"insert","predicate":"source","values":values}],"expect":{"echo":expect}})).unwrap()
}
#[test]
fn registration_is_not_execution_and_two_worlds_are_independent() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let definition = definition(&manager);
    assert_eq!(inventory(&mut manager)["worlds"], json!([]));
    let a = manager.create(definition.clone()).unwrap();
    let b = manager.create(definition).unwrap();
    assert_eq!(manager.status(&a).unwrap()["state"], "created");
    let sa = manager.start(&a).unwrap();
    let sb = manager.start(&b).unwrap();
    let pid = sa["resources"]["pid"].as_u64().unwrap() as i32;
    assert_ne!(sa["resources"]["pid"], sb["resources"]["pid"]);
    assert_eq!(sa["resources"]["state"], "available");
    assert!(sa["resources"]["resident_bytes"].as_u64().unwrap() > 0);
    assert!(manager.start(&a).is_err());
    assert_eq!(manager.status(&a).unwrap()["resources"]["pid"], pid);
    assert_eq!(manager.stop(&a).unwrap()["state"], "stopped");
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(manager.status(&b).unwrap()["state"], "running");
    let sbpid = sb["resources"]["pid"].as_u64().unwrap() as i32;
    drop(manager);
    assert_eq!(unsafe { libc::kill(sbpid, 0) }, -1);
}
#[test]
fn build_failure_and_child_exit_remain_observable() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    fs::write(f.root.join("reject_build"), "").unwrap();
    assert!(manager.start(&id).is_err());
    assert_eq!(manager.status(&id).unwrap()["state"], "failed");
    fs::remove_file(f.root.join("reject_build")).unwrap();
    let live = manager.start(&id).unwrap();
    let pid = live["resources"]["pid"].as_u64().unwrap() as i32;
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while manager.status(&id).unwrap()["state"] != "failed" {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // The dead child was reaped when its death was observed, so `start` (valid
    // for `failed`) opens a fresh generation without an explicit `stop`.
    let restarted = manager.start(&id).unwrap();
    assert_eq!(restarted["state"], "running");
    assert_eq!(restarted["generation"], 3);
    assert_ne!(restarted["resources"]["pid"], json!(pid));
    assert_eq!(manager.stop(&id).unwrap()["resources"]["state"], "missing");
    assert!(manager.status("stale-world").is_err());
}
#[test]
fn invalid_pin_never_creates_world() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let mut def = definition(&manager);
    def.processor.version = "invalid".into();
    assert!(manager.create(def).is_err());
    assert_eq!(inventory(&mut manager)["worlds"], json!([]));
}

#[test]
fn history_survives_owner_restart_and_second_owner_is_rejected() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def.clone()).unwrap();
    assert!(WorldManager::new(
        f.root.join("registry"),
        f.root.join("worlds"),
        f.root.join("build.py")
    )
    .is_err());
    manager.start(&id).unwrap();
    manager.stop(&id).unwrap();
    drop(manager);
    let mut recovered = f.manager();
    let status = recovered.status(&id).unwrap();
    assert_eq!(status["state"], "stopped");
    assert_eq!(
        status["definition"]["processor"]["version"],
        def.processor.version
    );
    assert_eq!(status["resources"]["pid"], serde_json::Value::Null);
    assert_eq!(status["history"].as_array().unwrap().len(), 4);
}
#[test]
fn interrupted_record_does_not_adopt_or_restart_a_process() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    drop(manager);
    let record = f.root.join("worlds").join(&id).join("world.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    value["state"] = json!("running");
    fs::write(&record, serde_json::to_vec(&value).unwrap()).unwrap();
    let mut recovered = f.manager();
    let status = recovered.status(&id).unwrap();
    assert_eq!(status["state"], "interrupted");
    assert_eq!(status["resources"]["pid"], serde_json::Value::Null);
    assert!(status["error"].as_str().unwrap().contains("no process"));
}

#[test]
fn library_registration_is_durable_and_does_not_execute() {
    let f = Fixture::new();
    let manager = f.manager();
    let library = manager
        .create_library(ddlog_runtime::worlds::LibraryDefinition {
            name: "Holocron".into(),
            repository: "https://github.com/VangelisTech/holocron".into(),
            revision: "example-pin".into(),
        })
        .unwrap();
    drop(manager);
    let mut manager = f.manager();
    assert_eq!(
        manager.libraries().unwrap()["libraries"][0]["id"],
        library["id"]
    );
    assert_eq!(inventory(&mut manager)["worlds"], json!([]));
}

#[test]
fn shutdown_is_terminal_for_new_start_requests() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    manager.shutdown_handle().stop_all();
    assert!(manager
        .start_async(&id)
        .unwrap_err()
        .contains("shutting down"));
    let status = manager.status(&id).unwrap();
    assert_eq!(status["state"], "created");
    assert_eq!(status["generation"], 0);
    assert_eq!(status["managed_processes"], json!([]));
}
#[test]
fn failed_persistence_retries_without_losing_history() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    let temp = f.root.join("worlds").join(&id).join("world.json.tmp");
    fs::create_dir(&temp).unwrap();
    assert!(manager.stop(&id).is_err());
    fs::remove_dir(&temp).unwrap();
    let status = manager.status(&id).unwrap();
    assert_eq!(status["state"], "stopped");
    let path = f.root.join("worlds").join(&id).join("world.json");
    let record: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(record["state"], "stopped");
    assert_eq!(record["history"].as_array().unwrap().len(), 2);
    assert_eq!(record["history"], status["history"]);
}
#[test]
fn failed_start_persistence_does_not_leave_phantom_starting_world() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    let temp = f.root.join("worlds").join(&id).join("world.json.tmp");
    fs::create_dir(&temp).unwrap();
    assert!(manager.start_async(&id).is_err());
    fs::remove_dir(&temp).unwrap();
    let status = manager.status(&id).unwrap();
    assert_eq!(status["state"], "failed");
    assert_eq!(status["managed_processes"], json!([]));
    assert!(status["error"].as_str().unwrap().contains("persist"));
    assert_eq!(manager.start(&id).unwrap()["state"], "running");
}

#[test]
fn runtime_info_reports_build_identity() {
    let info = ddlog_runtime::runtime_info();
    assert_eq!(info["schema_version"], 1);
    assert_eq!(info["crate_version"], env!("CARGO_PKG_VERSION"));
    assert!(info["dirty"].is_boolean());
    match &info["commit"] {
        Value::Null => (),
        Value::String(hash) => {
            assert!(hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        }
        other => panic!("unexpected commit {other}"),
    }
}

#[test]
fn registration_requires_names_and_files_definitions_into_libraries() {
    let f = Fixture::new();
    let manager = f.manager();
    let unnamed = manager.register(RegisterRequest {
        name: "  ".into(),
        description: String::new(),
        library_id: None,
        definition: echo_definition(),
        git_provenance: None,
    });
    assert!(unnamed.unwrap_err().contains("name"));
    assert!(manager
        .register(RegisterRequest {
            name: "Echo".into(),
            description: String::new(),
            library_id: Some("missing-library".into()),
            definition: echo_definition(),
            git_provenance: None,
        })
        .unwrap_err()
        .contains("Unknown library"));
    let echo = named(&manager, "Echo");
    let libraries = manager.libraries().unwrap();
    let unassigned = libraries["libraries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "unassigned")
        .unwrap();
    assert_eq!(unassigned["name"], "Unassigned");
    assert_eq!(
        unassigned["processors"][0]["processor_id"],
        echo.processor.processor_id
    );
    assert_eq!(unassigned["processors"][0]["name"], "Echo");
    let library = manager
        .create_library(ddlog_runtime::worlds::LibraryDefinition {
            name: "Examples".into(),
            repository: "https://example.invalid/examples".into(),
            revision: "main".into(),
        })
        .unwrap();
    assert_eq!(library["processors"], json!([]));
    // A pin belongs to exactly one library; association moves it.
    manager
        .associate(
            &echo.processor.processor_id,
            &echo.processor.version,
            library["id"].as_str().unwrap(),
            "Echo program",
            "Copies rows",
        )
        .unwrap();
    let libraries = manager.libraries().unwrap();
    for entry in libraries["libraries"].as_array().unwrap() {
        let held = entry["processors"].as_array().unwrap().len();
        assert_eq!(held, usize::from(entry["id"] == library["id"]), "{entry}");
    }
    // A composition of the interface-bearing program lists as a composition.
    let leaf = manager.register(RegisterRequest {
        name: "Leaf".into(),
        description: String::new(),
        library_id: None,
        definition: json!({"rules":"echo(N,S) :- source(N,S).","schemas":{"source":{"input":true,"fields":["int","string"]},"echo":{"input":false,"fields":["int","string"]}},"interface":{"inputs":["source"],"outputs":["echo"]}}),
        git_provenance: None,
    }).unwrap();
    let composed = manager.register(RegisterRequest {
        name: "Composed".into(),
        description: "wraps the leaf".into(),
        library_id: Some(library["id"].as_str().unwrap().into()),
        definition: json!({"composition":{"nodes":{"leaf":{"processor_id":leaf["processor_id"],"version":leaf["version"]}},"inputs":{"rows":{"fields":["int","string"],"targets":[{"node":"leaf","relation":"source"}]}},"bindings":[],"outputs":{"copies":{"node":"leaf","relation":"echo"}}}}),
        git_provenance: None,
    }).unwrap();
    assert_eq!(composed["library_id"], library["id"]);
    let listing = manager.definitions().unwrap();
    let rows = listing["processors"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let row = |pid: &Value| {
        rows.iter()
            .find(|row| row["processor_id"] == *pid)
            .unwrap()
            .clone()
    };
    let echo_row = row(&json!(echo.processor.processor_id));
    assert_eq!(echo_row["name"], "Echo program");
    assert_eq!(echo_row["description"], "Copies rows");
    assert_eq!(echo_row["library_id"], library["id"]);
    assert_eq!(echo_row["kind"], "program");
    assert_eq!(echo_row["current"], true);
    assert_eq!(echo_row["status"], "active");
    let composed_row = row(&composed["processor_id"]);
    assert_eq!(composed_row["kind"], "composition");
    assert_eq!(composed_row["name"], "Composed");
    assert_eq!(row(&leaf["processor_id"])["library_id"], "unassigned");
    let page = manager.registry().unwrap().list(100, None, false).unwrap();
    let summary = page
        .processors
        .iter()
        .find(|p| p.processor_id == composed["processor_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        summary.kind,
        ddlog_runtime::registry::ProcessorKind::Composition
    );
    // Every version of a processor is listed, and only one is current.
    let registry = manager.registry().unwrap();
    let published = registry.publish(echo.processor.processor_id.as_str(), serde_json::from_value(json!({"rules":"echo(N,S) :- source(N,S). twice(N) :- source(N,_).","schemas":{"source":{"input":true,"fields":["int","string"]},"echo":{"input":false,"fields":["int","string"]},"twice":{"input":false,"fields":["int"]}}})).unwrap(), &echo.processor.version, None).unwrap();
    let rows = manager.definitions().unwrap()["processors"].clone();
    let versions: Vec<&Value> = rows
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["processor_id"] == echo.processor.processor_id)
        .collect();
    assert_eq!(versions.len(), 2);
    let current: Vec<&&Value> = versions
        .iter()
        .filter(|row| row["current"] == true)
        .collect();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0]["version"], published.version);
    assert_eq!(
        current[0]["name"], "echo, twice ← source",
        "unassociated versions derive a name"
    );
}

#[test]
fn import_preserves_identities_from_a_public_fixture_registry() {
    let f = Fixture::new();
    let manager = f.manager();
    assert_eq!(
        fs::metadata(fixture_registry())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    let expected: Value = serde_json::from_slice(
        &fs::read(fixture_registry().parent().unwrap().join("records.json")).unwrap(),
    )
    .unwrap();
    let star = expected["star"]["processor_id"]
        .as_str()
        .unwrap()
        .to_string();
    let request = |dry_run: bool, processor_id: Option<&str>| ImportRequest {
        source_registry: fixture_registry(),
        processor_id: processor_id.map(String::from),
        library_id: None,
        names: [(star.clone(), "Star".to_string())].into_iter().collect(),
        dry_run,
    };
    let planned = manager.import(request(true, None)).unwrap();
    assert_eq!(planned["errors"], json!([]));
    assert_eq!(planned["dry_run"], true);
    assert_eq!(planned["imported"].as_array().unwrap().len(), 5);
    assert!(planned["imported"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["status"] == "imported"));
    assert_eq!(
        manager.definitions().unwrap()["processors"],
        json!([]),
        "dry run writes nothing"
    );
    let imported = manager.import(request(false, None)).unwrap();
    assert_eq!(imported["errors"], json!([]));
    assert_eq!(imported["imported"].as_array().unwrap().len(), 5);
    let registry = manager.registry().unwrap();
    for record in expected.as_object().unwrap().values() {
        let read = registry
            .get(
                record["processor_id"].as_str().unwrap(),
                Some(record["version"].as_str().unwrap()),
            )
            .unwrap();
        assert_eq!(
            serde_json::to_value(&read).unwrap(),
            *record,
            "identity and content preserved"
        );
        assert_eq!(
            registry
                .get(record["processor_id"].as_str().unwrap(), None)
                .unwrap()
                .version,
            record["version"],
            "current pointer copied"
        );
    }
    let listing = manager.definitions().unwrap();
    let rows = listing["processors"].as_array().unwrap();
    assert_eq!(rows.len(), 5);
    let name = |key: &str| {
        rows.iter()
            .find(|row| row["processor_id"] == expected[key]["processor_id"])
            .unwrap()["name"]
            .clone()
    };
    assert_eq!(name("star"), "Star");
    assert_eq!(name("nested"), "Composition of graph");
    assert_eq!(name("wrapper"), "Composition of graph");
    assert_eq!(name("pure"), "visible ← item");
    let again = manager.import(request(false, None)).unwrap();
    assert!(
        again["imported"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["status"] == "present"),
        "{again}"
    );
    // One processor imports its dependency closure first.
    let g = Fixture::new();
    let closure = g.manager();
    let nested = expected["nested"]["processor_id"].as_str().unwrap();
    let result = closure.import(request(false, Some(nested))).unwrap();
    let order: Vec<&str> = result["imported"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["processor_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        vec![
            expected["star"]["processor_id"].as_str().unwrap(),
            expected["wrapper"]["processor_id"].as_str().unwrap(),
            nested
        ]
    );
    assert_eq!(
        closure.definitions().unwrap()["processors"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    // A record with the same name but different content is never overwritten.
    let tampered = g.root.join("tampered");
    let copy = |from: &std::path::Path, to: &std::path::Path| {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                fs::create_dir_all(to.join(entry.file_name())).unwrap();
            }
        }
    };
    copy(&fixture_registry(), &tampered);
    for entry in fs::read_dir(fixture_registry()).unwrap() {
        let entry = entry.unwrap();
        let target = tampered.join(entry.file_name());
        fs::create_dir_all(target.join("versions")).unwrap();
        fs::copy(
            entry.path().join("current.json"),
            target.join("current.json"),
        )
        .unwrap();
        for version in fs::read_dir(entry.path().join("versions")).unwrap() {
            let version = version.unwrap();
            fs::copy(
                version.path(),
                target.join("versions").join(version.file_name()),
            )
            .unwrap();
        }
    }
    let star_dir = tampered.join(&star).join("versions");
    let star_file = fs::read_dir(&star_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut record: Value = serde_json::from_slice(&fs::read(&star_file).unwrap()).unwrap();
    record["created_at_unix_ms"] = json!(1);
    fs::write(&star_file, serde_json::to_vec(&record).unwrap()).unwrap();
    let conflict = closure
        .import(ImportRequest {
            source_registry: tampered.clone(),
            processor_id: None,
            library_id: None,
            names: Default::default(),
            dry_run: false,
        })
        .unwrap();
    assert_eq!(conflict["imported"], json!([]));
    assert_eq!(conflict["errors"].as_array().unwrap().len(), 1);
    assert_eq!(conflict["errors"][0]["processor_id"], star);
    assert!(conflict["errors"][0]["error"]
        .as_str()
        .unwrap()
        .contains("different content"));
    assert_eq!(
        closure.definitions().unwrap()["processors"]
            .as_array()
            .unwrap()
            .len(),
        3,
        "nothing written"
    );
    // A content-hash mismatch is rejected before anything is written.
    record["definition"]["rules"] = json!("changed(X) :- source(X).");
    fs::write(&star_file, serde_json::to_vec(&record).unwrap()).unwrap();
    let h = Fixture::new();
    let fresh = h.manager();
    let rejected = fresh
        .import(ImportRequest {
            source_registry: tampered,
            processor_id: None,
            library_id: None,
            names: Default::default(),
            dry_run: false,
        })
        .unwrap();
    assert!(
        rejected["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("hash mismatch"),
        "{rejected}"
    );
    assert_eq!(rejected["imported"], json!([]));
    assert_eq!(fresh.definitions().unwrap()["processors"], json!([]));
    assert!(fresh
        .import(ImportRequest {
            source_registry: "relative/path".into(),
            processor_id: None,
            library_id: None,
            names: Default::default(),
            dry_run: true
        })
        .is_err());
}

#[test]
fn inventory_filters_by_pin_and_summary_skips_live_instance_calls() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let echo = named(&manager, "Echo");
    let other = named(&manager, "Other");
    let a = manager.create(echo.clone()).unwrap();
    let b = manager.create(other.clone()).unwrap();
    let filtered = manager
        .inventory(&InventoryQuery {
            processor_id: Some(echo.processor.processor_id.clone()),
            version: None,
            summary: false,
        })
        .unwrap();
    assert_eq!(filtered["worlds"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["worlds"][0]["id"], a);
    let none = manager
        .inventory(&InventoryQuery {
            processor_id: Some(echo.processor.processor_id.clone()),
            version: Some("sha256:none".into()),
            summary: false,
        })
        .unwrap();
    assert_eq!(none["worlds"], json!([]));
    manager.start(&b).unwrap();
    let summary = manager
        .inventory(&InventoryQuery {
            processor_id: None,
            version: None,
            summary: true,
        })
        .unwrap();
    assert_eq!(summary["summary"], true);
    let running = summary["worlds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == b)
        .unwrap();
    assert_eq!(running["state"], "running");
    for absent in ["inspection", "instance", "managed_processes"] {
        assert!(running.get(absent).is_none(), "{absent} present in summary");
    }
    assert_eq!(
        running["persistence"],
        json!({"status":"not_configured","reason":"world checkpoints are not wired"})
    );
    assert_eq!(running["build"], Value::Null);
    assert_eq!(running["resources"]["state"], "available");
    let full = manager.status(&b).unwrap();
    let starting = full["history"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["state"] == "starting" && e["generation"] == 1)
        .unwrap();
    assert_eq!(full["started_at_unix_ms"], starting["at_unix_ms"]);
    assert_eq!(full["instance"]["revision"], 1);
    assert_eq!(
        full["definition"].get("purpose"),
        None,
        "default purpose is not serialized"
    );
    assert_eq!(
        manager.status(&a).unwrap()["started_at_unix_ms"],
        Value::Null
    );
    // A failed build reports its generation, log tail and instrumentation.
    fs::write(f.root.join("reject_build"), "").unwrap();
    assert!(manager.start(&a).is_err());
    let failed = manager.status(&a).unwrap();
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["build"]["generation"], 1);
    assert_eq!(failed["build"]["instrumented"], false);
    assert!(failed["build"]["log_tail"].is_string());
    let record: Value = serde_json::from_slice(
        &fs::read(f.root.join("worlds").join(&a).join("world.json")).unwrap(),
    )
    .unwrap();
    assert!(record.get("test").is_none());
}

#[test]
fn execute_exposes_revision_relations_typed_rows_and_source() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let echo = named(&manager, "Echo");
    let id = manager.create(echo).unwrap();
    let status = manager.start(&id).unwrap();
    let source =
        fs::read_to_string(f.root.join("worlds").join(&id).join("1/build-1/program.dl")).unwrap();
    use sha2::{Digest, Sha256};
    let sha = format!("{:x}", Sha256::digest(source.as_bytes()));
    assert_eq!(status["instance"]["revision"], 1);
    assert_eq!(status["instance"]["program_version"], 1);
    assert_eq!(status["instance"]["source_sha256"], sha);
    let program = manager.execute(&id, "program_source", &json!({})).unwrap();
    assert_eq!(program, json!({"source":source,"source_sha256":sha}));
    let applied = manager.execute(&id, "apply_changes", &json!({"changes":[{"op":"insert","predicate":"source","values":[1,"a"]},{"op":"insert","predicate":"source","values":[2,"b"]}]})).unwrap();
    assert_eq!(applied["revision"], 2);
    assert_eq!(manager.status(&id).unwrap()["instance"]["revision"], 2);
    let relations = manager.execute(&id, "relations", &json!({})).unwrap();
    assert_eq!(
        relations,
        json!({"revision":2,"relations":[{"name":"source","input":true,"fields":["int","string"],"count":2},{"name":"echo","input":false,"fields":["int","string"],"count":2}]})
    );
    let first = manager
        .execute(&id, "query_rows", &json!({"predicate":"echo","max_rows":1}))
        .unwrap();
    assert_eq!(first["rows"], json!([[1, "a"]]));
    assert_eq!(first["total"], 2);
    assert_eq!(first["complete"], false);
    assert_eq!(first["fields"], json!(["int", "string"]));
    assert_eq!(first["revision"], 2);
    let second = manager
        .execute(
            &id,
            "query_rows",
            &json!({"predicate":"echo","max_rows":1,"continuation":first["continuation"]}),
        )
        .unwrap();
    assert_eq!(second["rows"], json!([[2, "b"]]));
    assert_eq!(second["complete"], true);
    assert_eq!(second["continuation"], Value::Null);
    let inputs = manager
        .execute(
            &id,
            "query_rows",
            &json!({"predicate":"source","max_rows":1}),
        )
        .unwrap();
    assert_eq!(inputs["rows"], json!([[1, "a"]]));
    assert_eq!(inputs["total"], 2);
    assert_eq!(inputs["complete"], false);
    let rest = manager
        .execute(
            &id,
            "query_rows",
            &json!({"predicate":"source","continuation":inputs["continuation"]}),
        )
        .unwrap();
    assert_eq!(rest["rows"], json!([[2, "b"]]));
    assert_eq!(rest["complete"], true);
    let everything = manager
        .execute(&id, "query_rows", &json!({"predicate":"echo"}))
        .unwrap();
    assert_eq!(everything["rows"].as_array().unwrap().len(), 2);
    assert!(manager
        .execute(&id, "query_rows", &json!({"predicate":"echo","max_rows":0}))
        .unwrap_err()
        .contains("max_rows"));
    assert!(manager
        .execute(&id, "query_rows", &json!({"predicate":"missing"}))
        .unwrap_err()
        .contains("Unknown public relation"));
    manager
        .execute(
            &id,
            "apply_changes",
            &json!({"changes":[{"op":"delete","predicate":"source","values":[1,"a"]}]}),
        )
        .unwrap();
    assert!(manager
        .execute(
            &id,
            "query_rows",
            &json!({"predicate":"echo","continuation":first["continuation"]})
        )
        .unwrap_err()
        .contains("Continuation"));
    assert!(manager
        .execute(
            &id,
            "query_rows",
            &json!({"predicate":"source","continuation":inputs["continuation"]})
        )
        .unwrap_err()
        .contains("Continuation"));
}

#[test]
fn scenarios_are_validated_against_public_relations() {
    let f = Fixture::new();
    let manager = f.manager();
    let record = manager.register(RegisterRequest {
        name: "Bounded".into(),
        description: String::new(),
        library_id: None,
        definition: json!({"rules":"echo(N,S) :- source(N,S). private(N) :- source(N,_).","schemas":{"source":{"input":true,"fields":["int","string"]},"echo":{"input":false,"fields":["int","string"]},"private":{"input":false,"fields":["int"]}},"interface":{"inputs":["source"],"outputs":["echo"]}}),
        git_provenance: None,
    }).unwrap();
    let (pid, version) = (
        record["processor_id"].as_str().unwrap(),
        record["version"].as_str().unwrap(),
    );
    assert_eq!(
        manager.scenarios_get(pid, version).unwrap()["scenarios"],
        json!([])
    );
    let good = vec![scenario(
        "one",
        vec![json!(1), json!("a")],
        vec![vec![json!(1), json!("a")]],
    )];
    let stored = manager.scenarios_set(pid, version, good.clone()).unwrap();
    assert_eq!(stored["processor"]["processor_id"], pid);
    assert_eq!(
        manager.scenarios_get(pid, version).unwrap()["scenarios"],
        json!(good)
    );
    let bad = |value: Value| -> String {
        let scenarios: Vec<Scenario> = serde_json::from_value(json!([value])).unwrap();
        manager.scenarios_set(pid, version, scenarios).unwrap_err()
    };
    assert!(bad(json!({"name":"x","changes":[{"op":"insert","predicate":"echo","values":[1,"a"]}],"expect":{}})).contains("not a public input"));
    assert!(bad(json!({"name":"x","changes":[{"op":"upsert","predicate":"source","values":[1,"a"]}],"expect":{}})).contains("insert or delete"));
    assert!(bad(json!({"name":"x","changes":[{"op":"insert","predicate":"source","values":[1]}],"expect":{}})).contains("arity"));
    assert!(bad(json!({"name":"x","changes":[{"op":"insert","predicate":"source","values":["1","a"]}],"expect":{}})).contains("must be int"));
    assert!(
        bad(json!({"name":"x","changes":[],"expect":{"private":[[1]]}})).contains("not public")
    );
    assert!(
        bad(json!({"name":"x","changes":[],"expect":{"echo":[[1,"a"],[1,"a"]]}}))
            .contains("duplicate")
    );
    assert!(bad(json!({"name":"","changes":[],"expect":{}})).contains("name"));
    let duplicate: Vec<Scenario> = serde_json::from_value(
        json!([{"name":"x","changes":[],"expect":{}},{"name":"x","changes":[],"expect":{}}]),
    )
    .unwrap();
    assert!(manager
        .scenarios_set(pid, version, duplicate)
        .unwrap_err()
        .contains("Duplicate"));
    assert!(serde_json::from_value::<Scenario>(
        json!({"name":"x","changes":[],"expect":{},"extra":1})
    )
    .is_err());
    assert!(manager
        .scenarios_set(
            pid,
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            vec![]
        )
        .is_err());
    assert_eq!(
        manager.scenarios_get(pid, version).unwrap()["scenarios"],
        json!(good),
        "rejected writes leave the store unchanged"
    );
}

#[test]
fn test_worlds_run_scenarios_asynchronously_and_report_results() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let echo = named(&manager, "Echo");
    let (pid, version) = (
        echo.processor.processor_id.clone(),
        echo.processor.version.clone(),
    );
    let request = |scenarios: Option<Vec<&str>>, keep_world: bool| TestRequest {
        processor_id: pid.clone(),
        version: version.clone(),
        scenarios: scenarios.map(|names| names.into_iter().map(String::from).collect()),
        keep_world,
    };
    assert!(manager
        .test(request(None, false))
        .unwrap_err()
        .contains("scenarios_set"));
    manager
        .scenarios_set(
            &pid,
            &version,
            vec![
                scenario(
                    "first",
                    vec![json!(1), json!("a")],
                    vec![vec![json!(1), json!("a")]],
                ),
                scenario(
                    "second",
                    vec![json!(2), json!("b")],
                    vec![vec![json!(1), json!("a")], vec![json!(2), json!("b")]],
                ),
                scenario(
                    "wrong",
                    vec![json!(3), json!("c")],
                    vec![vec![json!(1), json!("a")], vec![json!(9), json!("z")]],
                ),
            ],
        )
        .unwrap();
    assert!(manager
        .test(request(Some(vec!["missing"]), false))
        .unwrap_err()
        .contains("Unknown scenario"));
    let started = manager.test(request(None, false)).unwrap();
    assert_eq!(started["state"], "starting");
    assert_eq!(started["definition"]["purpose"], "test");
    assert_eq!(started["definition"]["label"], "Test · Echo");
    assert_eq!(
        started["definition"]["scenarios"].as_array().unwrap().len(),
        3
    );
    assert_eq!(started["test"]["phase"], "building");
    assert_eq!(started["test"]["passed"], Value::Null);
    let id = started["id"].as_str().unwrap().to_string();
    let done = wait_until(&mut manager, &id, |status| status["state"] != "starting");
    assert_eq!(
        done["state"], "stopped",
        "the instance is dropped when keep_world is false: {done}"
    );
    assert_eq!(done["error"], Value::Null);
    let test = &done["test"];
    assert_eq!(test["phase"], "done");
    assert_eq!(test["scenario_index"], 3);
    assert_eq!(test["passed"], false);
    let results = test["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["name"], "first");
    assert_eq!(results[0]["passed"], true);
    assert_eq!(results[0]["revision"], 2);
    assert_eq!(results[0]["observed"]["echo"], json!([[1, "a"]]));
    assert_eq!(results[1]["passed"], true);
    assert_eq!(results[1]["revision"], 3);
    assert_eq!(results[1]["observed"]["echo"], json!([[1, "a"], [2, "b"]]));
    assert_eq!(results[2]["passed"], false);
    assert_eq!(results[2]["missing"]["echo"], json!([[9, "z"]]));
    assert_eq!(
        results[2]["unexpected"]["echo"],
        json!([[2, "b"], [3, "c"]])
    );
    assert_eq!(results[2]["error"], Value::Null);
    assert_eq!(done["resources"]["pid"], Value::Null);
    let record: Value = serde_json::from_slice(
        &fs::read(f.root.join("worlds").join(&id).join("world.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["test"], *test, "test results are persisted");
    let listed = inventory(&mut manager);
    let world = listed["worlds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == id)
        .unwrap();
    assert_eq!(world["definition"]["purpose"], "test");
    // Selected scenarios only, keeping the world for inspection.
    let kept = manager.test(request(Some(vec!["second"]), true)).unwrap();
    let kept_id = kept["id"].as_str().unwrap().to_string();
    let running = wait_until(&mut manager, &kept_id, |status| {
        status["state"] != "starting"
    });
    assert_eq!(running["state"], "running");
    assert_eq!(
        running["test"]["passed"], false,
        "the second scenario alone lacks the first insert"
    );
    assert_eq!(running["test"]["results"].as_array().unwrap().len(), 1);
    assert_eq!(running["test"]["results"][0]["name"], "second");
    assert_eq!(
        running["test"]["results"][0]["missing"]["echo"],
        json!([[1, "a"]])
    );
    assert_eq!(
        manager.execute(&kept_id, "relations", &json!({})).unwrap()["relations"][0]["count"],
        1
    );
    assert_eq!(manager.stop(&kept_id).unwrap()["state"], "stopped");
    // Build failures are reported as a failed test phase with the build error.
    fs::write(f.root.join("reject_build"), "").unwrap();
    let broken = manager.test(request(Some(vec!["first"]), false)).unwrap();
    let broken_id = broken["id"].as_str().unwrap().to_string();
    let failed = wait_until(&mut manager, &broken_id, |status| {
        status["state"] != "starting"
    });
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["test"]["phase"], "failed");
    assert_eq!(failed["test"]["passed"], false);
    assert!(failed["test"]["error"]
        .as_str()
        .unwrap()
        .contains("compilation failed"));
    assert_eq!(failed["build"]["generation"], 1);
    fs::remove_file(f.root.join("reject_build")).unwrap();
    // Purpose and scenarios are validated at creation.
    let mut invalid = echo.clone();
    invalid.purpose = "benchmark".into();
    assert!(manager.create(invalid).unwrap_err().contains("purpose"));
    let mut empty = echo.clone();
    empty.purpose = "test".into();
    assert!(manager
        .create(empty)
        .unwrap_err()
        .contains("at least one scenario"));
    let mut instance = echo.clone();
    instance.scenarios = vec![scenario("s", vec![json!(1), json!("a")], vec![])];
    assert!(manager
        .create(instance)
        .unwrap_err()
        .contains("purpose test"));
    // Results survive owner restart.
    drop(manager);
    let mut recovered = f.manager();
    let status = recovered.status(&id).unwrap();
    assert_eq!(status["state"], "stopped");
    assert_eq!(status["test"]["phase"], "done");
    assert_eq!(status["test"]["results"].as_array().unwrap().len(), 3);
}

/// Append synthetic native capture lines; the simulated runtime never writes them.
fn append_capture(f: &Fixture, id: &str, generation: u64, events: &[Value]) {
    use std::io::Write;
    let path = f
        .root
        .join("worlds")
        .join(id)
        .join(generation.to_string())
        .join("native-events.jsonl");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for event in events {
        writeln!(file, "{event}").unwrap();
    }
}
fn operates(id: u64, addr: &[u64], name: &str, debug: &str) -> Value {
    json!({"stream":"timely","worker":0,"time_ns":id,"event":{"Operates":{"id":id,"addr":addr,"name":name}},"debug":debug})
}
fn schedule(id: u64, start: u64, stop: u64) -> Vec<Value> {
    vec![
        json!({"stream":"timely","worker":0,"time_ns":start,"event":{"Schedule":{"id":id,"start_stop":"Start"}}}),
        json!({"stream":"timely","worker":0,"time_ns":stop,"event":{"Schedule":{"id":id,"start_stop":"Stop"}}}),
    ]
}
fn topology() -> Vec<Value> {
    vec![
        operates(1, &[0, 1], "Input", "Input { rel: \"R_source\" }"),
        operates(2, &[0, 2], "Map", "authored"),
        json!({"stream":"timely","worker":0,"time_ns":3,"event":{"Channels":{"id":9,"scope_addr":[0],"source":[1,0],"target":[2,0]}}}),
    ]
}
fn capture_file(f: &Fixture, definition: &WorldDefinition) -> PathBuf {
    f.root
        .join("worlds")
        .join("captures")
        .join(&definition.processor.processor_id)
        .join(format!("{}.json", &definition.processor.version[7..]))
}
#[test]
fn activity_is_tailed_rotation_keeps_topology_and_captures_are_retained_once() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let echo = named(&manager, "Echo");
    let (pid, version) = (
        echo.processor.processor_id.clone(),
        echo.processor.version.clone(),
    );
    assert_eq!(
        manager.capture_get(&pid, &version).unwrap()["state"],
        "missing"
    );
    assert!(manager.capture_get("processor_missing", &version).is_err());
    let id = manager.create(echo.clone()).unwrap();
    let running = manager.start(&id).unwrap();
    assert_eq!(
        running["inspection"]["state"], "missing",
        "the simulated runtime writes no capture"
    );
    assert!(!capture_file(&f, &echo).exists());
    // Topology alone is available but not yet captured: no Schedule event.
    append_capture(&f, &id, 1, &topology());
    let available = wait_until(&mut manager, &id, |s| {
        s["inspection"]["state"] == "available"
    });
    assert_eq!(available["inspection"]["unresolved_channels"], 0);
    assert_eq!(available["inspection"]["activity"]["complete"], true);
    assert_eq!(
        available["inspection"]["activity"]["totals"]["operators"],
        2
    );
    assert_eq!(
        available["inspection"]["activity"]["nodes"]["0:2"]["schedule_count"],
        0
    );
    assert!(!capture_file(&f, &echo).exists());
    let mut events = schedule(2, 100, 250);
    events.push(json!({"stream":"timely","worker":0,"time_ns":300,"event":{"Messages":{"is_send":true,"channel":9,"source":0,"target":0,"seq_no":1,"length":7}}}));
    events.push(json!({"stream":"progress","worker":0,"time_ns":310,"event":{}}));
    events.push(json!({"stream":"differential","worker":0,"time_ns":320,"event":{"kind":"Batch","operator":2,"length":3}}));
    events.push(json!({"stream":"timely","worker":0,"time_ns":400,"event":{"Shutdown":{"id":1}}}));
    append_capture(&f, &id, 1, &events);
    let active = wait_until(&mut manager, &id, |s| {
        s["inspection"]["activity"]["nodes"]["0:2"]["schedule_count"] == 1
    });
    let activity = &active["inspection"]["activity"];
    assert_eq!(
        activity["nodes"]["0:2"],
        json!({"schedule_count":1,"busy_ns":150,"last_seen_ns":250,"active":true,"arrangement_events":1,"last_arrangement_event":{"kind":"Batch","time_ns":320,"length":3}})
    );
    assert_eq!(activity["nodes"]["0:1"]["active"], false);
    assert_eq!(
        activity["channels"]["0:9"],
        json!({"message_count":1,"records":7})
    );
    assert_eq!(activity["totals"]["timely"], 7);
    assert_eq!(activity["totals"]["progress"], 1);
    assert_eq!(activity["totals"]["differential"], 1);
    assert_eq!(activity["last_event_ns"], 400);
    assert_eq!(activity["rotations"], 0);
    assert_eq!(activity["lag_bytes"], 0);
    assert!(activity["last_ingest_unix_ms"].as_u64().unwrap() > 0);
    // The first available status with a Schedule event retained the capture.
    let capture = manager.capture_get(&pid, &version).unwrap();
    assert_eq!(capture["schema_version"], 1);
    assert_eq!(capture["world_id"], id);
    assert_eq!(capture["generation"], 1);
    assert_eq!(capture["processor"], json!(echo.processor));
    assert_eq!(capture["graph"]["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(capture["graph"]["edges"].as_array().unwrap().len(), 1);
    assert_eq!(capture["unresolved_channels"], 0);
    assert!(capture.get("activity").is_none(), "topology only");
    let captured_at = capture["captured_at_unix_ms"].as_u64().unwrap();
    assert!(captured_at > 0);
    let record: Value = serde_json::from_slice(
        &fs::read(f.root.join("worlds").join(&id).join("world.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["capture_generation"], 1);
    // Rotation: the hook truncated the file and wrote the rotation record first.
    let mut rotated = vec![json!({"stream":"capture_status","status":"rotated","bytes":4096})];
    rotated.extend(schedule(2, 500, 600));
    let path = f
        .root
        .join("worlds")
        .join(&id)
        .join("1")
        .join("native-events.jsonl");
    fs::write(&path, "").unwrap();
    append_capture(&f, &id, 1, &rotated);
    let after = wait_until(&mut manager, &id, |s| {
        s["inspection"]["activity"]["rotations"] == 1
            && s["inspection"]["activity"]["nodes"]["0:2"]["schedule_count"] == 2
    });
    assert_eq!(after["inspection"]["state"], "available");
    assert_eq!(
        after["inspection"]["graph"]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        after["inspection"]["activity"]["nodes"]["0:2"]["busy_ns"],
        250
    );
    assert_eq!(after["inspection"]["activity"]["complete"], true);
    // Captures are retained once per generation: nothing was rewritten.
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert_eq!(
        manager.capture_get(&pid, &version).unwrap()["captured_at_unix_ms"],
        captured_at
    );
    // A truncation marker is reported as a state; lag never is.
    append_capture(
        &f,
        &id,
        1,
        &[json!({"stream":"capture_status","status":"truncated","reason":"worker_byte_limit"})],
    );
    let truncated = wait_until(&mut manager, &id, |s| {
        s["inspection"]["state"] == "truncated"
    });
    assert_eq!(truncated["inspection"]["activity"]["complete"], false);
    assert!(
        truncated["inspection"]["activity"]["truncated_at_bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(truncated["state"], "running");
    // Summary inventory never touches the capture.
    let summary = manager
        .inventory(&InventoryQuery {
            processor_id: None,
            version: None,
            summary: true,
        })
        .unwrap();
    assert!(summary["worlds"][0].get("inspection").is_none());
    // Stopping joins the tailer; the ingested state stays readable.
    let stopped = manager.stop(&id).unwrap();
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["inspection"]["activity"]["rotations"], 1);
    // A second generation captures again and replaces the retained document.
    fs::remove_file(&path).unwrap();
    manager.start(&id).unwrap();
    let mut second = topology();
    second.extend(schedule(2, 10, 20));
    append_capture(&f, &id, 2, &second);
    wait_until(&mut manager, &id, |s| {
        s["inspection"]["activity"]["nodes"]["0:2"]["schedule_count"] == 1
    });
    let capture = manager.capture_get(&pid, &version).unwrap();
    assert_eq!(capture["generation"], 2);
    assert_eq!(manager.stop(&id).unwrap()["state"], "stopped");
    // Recovered worlds read their last capture once and hold it without a tailer.
    drop(manager);
    let mut recovered = f.manager();
    let status = recovered.status(&id).unwrap();
    assert_eq!(status["state"], "stopped");
    assert_eq!(status["inspection"]["state"], "available");
    assert_eq!(
        status["inspection"]["activity"]["nodes"]["0:2"]["schedule_count"],
        1
    );
    assert_eq!(status["inspection"]["activity"]["complete"], true);
    assert_eq!(
        recovered.capture_get(&pid, &version).unwrap()["generation"],
        2,
        "recovery does not re-capture a retained generation"
    );
    append_capture(&f, &id, 2, &schedule(2, 30, 40));
    std::thread::sleep(std::time::Duration::from_millis(250));
    assert_eq!(
        recovered.status(&id).unwrap()["inspection"]["activity"]["nodes"]["0:2"]["schedule_count"],
        1,
        "a stopped world's capture is read once, never tailed"
    );
}

#[test]
fn member_match_groups_resolve_at_snapshot_time() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let provenance = json!({"repository":"repo","revision":"commit","source":null});
    let matched = with_groups(
        &manager,
        json!([
            {"id":"phase","name":"Large star","member_key":"large","provenance":provenance,
             "member_match":{"scope_name":"large-star"},
             "ports":[{"id":"in","name":"in","direction":"input","data_type":"edge",
                       "native_match":{"debug_pattern":"scope$","index":0}}]},
            {"id":"transformer","name":"Transformer","member_key":"transformer","provenance":provenance,
             "member_match":{"debug_pattern":"ApplyTransformer \\{ transformer: \"Star\""}}
        ]),
    );
    let stored = manager
        .registry()
        .unwrap()
        .get(
            &matched.processor.processor_id,
            Some(&matched.processor.version),
        )
        .unwrap();
    let stored = serde_json::to_value(&stored).unwrap();
    assert_eq!(
        stored["definition"]["inspection"]["authoredGroups"][0]["memberIds"],
        json!([])
    );
    assert_eq!(
        stored["definition"]["inspection"]["authoredGroups"][0]["member_match"],
        json!({"scope_name":"large-star"})
    );
    assert!(with_groups_fails(&manager, json!([{"id":"bad","name":"Bad","member_key":"bad","provenance":provenance,"member_match":{}}])).contains("member_match"));
    assert!(with_groups_fails(&manager, json!([{"id":"bad","name":"Bad","member_key":"bad","provenance":provenance,"member_match":{"debug_pattern":"("}}])).contains("debug_pattern"));
    let id = manager.create(matched.clone()).unwrap();
    manager.start(&id).unwrap();
    let nodes = vec![
        operates(0, &[0], "Dataflow", ""),
        operates(1, &[0, 1], "Input", "Input { rel: \"R_source\" }"),
        operates(
            2,
            &[0, 2],
            "large-star",
            "ApplyTransformer { transformer: \"Star\" } scope",
        ),
        operates(
            3,
            &[0, 2, 1],
            "Map",
            "ApplyTransformer { transformer: \"Star\" } map",
        ),
        operates(
            4,
            &[0, 2, 2],
            "Reduce",
            "ApplyTransformer { transformer: \"Star\" }",
        ),
        operates(
            5,
            &[0, 3],
            "Probe",
            "ApplyTransformer { transformer: \"Star\" }",
        ),
        json!({"stream":"timely","worker":0,"time_ns":9,"event":{"Channels":{"id":9,"scope_addr":[0],"source":[1,0],"target":[2,0]}}}),
    ];
    append_capture(&f, &id, 1, &nodes);
    let status = wait_until(&mut manager, &id, |s| {
        s["inspection"]["state"] == "available"
    });
    assert_eq!(status["inspection"]["mapping_error"], Value::Null);
    let groups = &status["inspection"]["metadata"]["authoredGroups"];
    assert_eq!(groups[0]["memberIds"], json!(["0:2", "0:3", "0:4"]));
    assert_eq!(
        groups[0]["ports"][0]["native_ports"],
        json!([{"operator_id":"0:2","index":0}])
    );
    assert_eq!(groups[1]["memberIds"], json!(["0:5"]));
    append_capture(&f, &id, 1, &schedule(3, 1, 2));
    wait_until(&mut manager, &id, |_| manager_capture_exists(&f, &matched));
    let capture = manager
        .capture_get(&matched.processor.processor_id, &matched.processor.version)
        .unwrap();
    assert_eq!(
        capture["metadata"]["authoredGroups"][0]["memberIds"],
        json!(["0:2", "0:3", "0:4"])
    );
    assert_eq!(capture["mapping_error"], Value::Null);
    manager.stop(&id).unwrap();
    // Zero matches are a mapping error naming the group; memberIds stay empty.
    let missing = with_groups(
        &manager,
        json!([
            {"id":"absent","name":"Absent","member_key":"absent","provenance":provenance,
             "member_match":{"scope_name":"small-star"}}
        ]),
    );
    let id = manager.create(missing.clone()).unwrap();
    manager.start(&id).unwrap();
    append_capture(&f, &id, 1, &nodes);
    append_capture(&f, &id, 1, &schedule(3, 1, 2));
    let status = wait_until(&mut manager, &id, |s| {
        s["inspection"]["state"] == "available"
    });
    let error = status["inspection"]["mapping_error"].as_str().unwrap();
    assert!(
        error.contains("absent") && error.contains("small-star"),
        "{error}"
    );
    assert_eq!(
        status["inspection"]["metadata"]["authoredGroups"][0]["memberIds"],
        json!([])
    );
    let capture = wait_until(&mut manager, &id, |_| manager_capture_exists(&f, &missing));
    assert_eq!(capture["state"], "running");
    let capture = manager
        .capture_get(&missing.processor.processor_id, &missing.processor.version)
        .unwrap();
    assert!(capture["mapping_error"]
        .as_str()
        .unwrap()
        .contains("absent"));
    manager.stop(&id).unwrap();
}
fn with_groups(manager: &WorldManager, groups: Value) -> WorldDefinition {
    let mut definition = echo_definition();
    definition["inspection"] = json!({"schema_version":1,"authoredGroups":groups});
    let record = manager
        .register(RegisterRequest {
            name: "Matched".into(),
            description: String::new(),
            library_id: None,
            definition,
            git_provenance: None,
        })
        .unwrap();
    WorldDefinition {
        label: "Matched".into(),
        processor: ProcessorReference {
            processor_id: record["processor_id"].as_str().unwrap().into(),
            version: record["version"].as_str().unwrap().into(),
        },
        purpose: "instance".into(),
        scenarios: vec![],
    }
}
fn with_groups_fails(manager: &WorldManager, groups: Value) -> String {
    let mut definition = echo_definition();
    definition["inspection"] = json!({"schema_version":1,"authoredGroups":groups});
    manager
        .register(RegisterRequest {
            name: "Bad".into(),
            description: String::new(),
            library_id: None,
            definition,
            git_provenance: None,
        })
        .unwrap_err()
}
fn manager_capture_exists(f: &Fixture, definition: &WorldDefinition) -> bool {
    capture_file(f, definition).exists()
}
