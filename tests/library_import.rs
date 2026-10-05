#![cfg(unix)]
//! Saved-artifact admission: no compiler or execution process is used.
use ddlog_runtime::registry::{ProcessorDefinition, ProcessorRegistry, ProcessorVersion};
use ddlog_runtime::worlds::{InventoryQuery, LibraryImportRequest, WorldManager};
use serde_json::{json, Value};
use std::{fs, path::PathBuf};
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "library-import-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn registry(&self, name: &str) -> ProcessorRegistry {
        ProcessorRegistry::open(self.0.join(name)).unwrap()
    }
    fn manager(&self) -> WorldManager {
        WorldManager::new(
            self.0.join("destination"),
            self.0.join("worlds"),
            "/usr/bin/false".into(),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn records(registry: &ProcessorRegistry) -> (ProcessorVersion, ProcessorVersion) {
    let leaf: ProcessorDefinition = serde_json::from_value(json!({
        "rules":"out(X) :- source(X).", "schemas":{"source":{"input":true,"fields":["int"]},"out":{"input":false,"fields":["int"]}},
        "interface":{"inputs":["source"],"outputs":["out"]}
    })).unwrap();
    let child = registry.create(leaf, None).unwrap();
    let parent = registry.create(serde_json::from_value(json!({"composition":{
        "nodes":{"child":{"processor_id":child.processor_id,"version":child.version}},
        "inputs":{"input":{"fields":["int"],"targets":[{"node":"child","relation":"source"}]}},
        "bindings":[],"outputs":{"result":{"node":"child","relation":"out"}}
    }})).unwrap(), None).unwrap();
    (child, parent)
}
fn artifact(child: &ProcessorVersion, parent: &ProcessorVersion) -> Value {
    json!({"revision":"explicit-test-revision","artifact":{
        "schema_version":1,"library":{"key":"test","name":"Test","repository":"test://source","package":"test","package_version":"1"},
        "entries":[
            {"key":"parent","name":"Parent","description":"Composed program","source":["programs/parent.dl"],"git_provenance":null,"record":parent,
             "scenarios":[{"name":"one","description":"One cumulative change","changes":[{"op":"insert","predicate":"input","values":[1]}],"expect":{"result":[[1]]}}]},
            {"key":"child","name":"Child","description":"Leaf","source":[],"git_provenance":null,"record":child,"scenarios":[]}
        ],
        "pins":{"parent":{"processor_id":parent.processor_id,"version":parent.version},"child":{"processor_id":child.processor_id,"version":child.version}}
    }})
}
fn request(value: Value) -> LibraryImportRequest {
    serde_json::from_value(value).unwrap()
}

#[test]
fn imported_records_validate_full_closure_before_writes_and_keep_current() {
    let fixture = Fixture::new();
    let source = fixture.registry("source");
    let (child, parent) = records(&source);
    let destination = fixture.registry("destination");
    let missing = destination
        .import_records(vec![parent.clone()], false)
        .unwrap();
    assert!(!missing.errors.is_empty());
    assert_eq!(
        fs::read_dir(fixture.0.join("destination")).unwrap().count(),
        0
    );
    let report = destination
        .import_records(vec![parent.clone(), child.clone()], true)
        .unwrap();
    assert!(report.errors.is_empty());
    assert_eq!(
        fs::read_dir(fixture.0.join("destination")).unwrap().count(),
        0
    );
    let report = destination
        .import_records(vec![parent.clone(), child.clone()], false)
        .unwrap();
    assert!(report.errors.is_empty());
    assert_eq!(report.imported[0].record, child);
    assert_eq!(destination.get(&parent.processor_id, None).unwrap(), parent);
    let mut changed = child.definition.clone();
    if let ProcessorDefinition::Program(p) = &mut changed {
        p.rules.push('\n');
    }
    let newer = destination
        .publish(&child.processor_id, changed, &child.version, None)
        .unwrap();
    destination
        .import_records(vec![child.clone(), parent], false)
        .unwrap();
    assert_eq!(destination.get(&child.processor_id, None).unwrap(), newer);
}

#[test]
fn library_import_is_idempotent_and_creates_no_world_or_build() {
    let fixture = Fixture::new();
    let (child, parent) = records(&fixture.registry("source"));
    let mut manager = fixture.manager();
    let value = artifact(&child, &parent);
    let mut dry = value.clone();
    dry["dry_run"] = json!(true);
    assert_eq!(
        manager.library_import(request(dry)).unwrap()["dry_run"],
        true
    );
    assert_eq!(
        manager.libraries().unwrap()["libraries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let first = manager.library_import(request(value.clone())).unwrap();
    assert_eq!(manager.library_import(request(value)).unwrap(), first);
    assert_eq!(
        manager.inventory(&InventoryQuery::default()).unwrap()["worlds"],
        json!([])
    );
    let libraries = manager.libraries().unwrap();
    let library = libraries["libraries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == first["library_id"])
        .unwrap();
    assert_eq!(library["processors"].as_array().unwrap().len(), 2);
    assert_eq!(
        manager
            .scenarios_get(&parent.processor_id, &parent.version)
            .unwrap()["scenarios"][0]["name"],
        "one"
    );
    assert_eq!(
        manager
            .registry()
            .unwrap()
            .get(&parent.processor_id, Some(&parent.version))
            .unwrap(),
        parent
    );
}

#[test]
fn invalid_scenarios_pins_provenance_and_records_leave_no_publication() {
    let fixture = Fixture::new();
    let (child, parent) = records(&fixture.registry("source"));
    let manager = fixture.manager();
    let baseline = artifact(&child, &parent);
    for changed in [0, 1, 2, 3, 4] {
        let mut value = baseline.clone();
        match changed {
            0 => {
                value["artifact"]["entries"][0]["scenarios"][0]["changes"][0]["values"] =
                    json!(["wrong type"])
            }
            1 => value["artifact"]["pins"]["parent"]["version"] = json!(child.version),
            2 => {
                value["artifact"]["entries"][0]["git_provenance"] =
                    json!({"repository":"wrong","revision":"wrong"})
            }
            3 => {
                value["artifact"]["entries"][1]["record"]["definition"]["rules"] =
                    json!("out(X) :- source(X), X > 0.")
            }
            _ => value["artifact"]["entries"][0]["source"] = json!(["../../outside"]),
        }
        assert!(manager.library_import(request(value)).is_err());
        assert_eq!(
            fs::read_dir(fixture.0.join("destination")).unwrap().count(),
            0
        );
        assert!(!fixture.0.join("worlds/libraries.json").exists());
        assert!(!fixture.0.join("worlds/scenarios").exists());
    }
}
