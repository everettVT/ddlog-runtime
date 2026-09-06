#![cfg(unix)]
//! Controlled transport/compiler fixtures, never claimed as native DDlog proof.
use ddlog_runtime::Backend;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ddlog-memory-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let template =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/memory_fake_runtime.py");
        let script = format!("#!/usr/bin/env python3\nimport sys\nfrom pathlib import Path\nroot=Path({})\nif (root/'reject_build').exists(): sys.exit(91)\nsource=Path({}).read_text().replace('__CONTROL__', {})\nPath(sys.argv[2]).write_text(source)\nPath(sys.argv[2]).chmod(0o700)\n", json!(root), json!(template), json!(serde_json::to_string(&root).unwrap()));
        fs::write(root.join("build.py"), script).unwrap();
        fs::set_permissions(root.join("build.py"), fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }
    fn backend(&self, name: &str) -> Backend {
        Backend::new(self.root.join(name), self.root.join("build.py"))
    }
    fn flag(&self, name: &str) {
        fs::write(self.root.join(name), "").unwrap();
    }
    fn unflag(&self, name: &str) {
        fs::remove_file(self.root.join(name)).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn schemas() -> Value {
    json!({
        "source":{"input":true,"fields":["int","string"]},
        "unused":{"input":true,"fields":["string"]},
        "echo":{"input":false,"fields":["int","string"]}
    })
}
fn initial(f: &Fixture) -> Backend {
    let mut backend = f.backend("live");
    backend
        .install("echo(N,S) :- source(N,S).", schemas())
        .unwrap();
    backend
        .apply(&json!([{"op":"insert","predicate":"source","values":[-7,"a\n\"b\"\\é"]}]))
        .unwrap();
    backend
}
fn rewrite_checkpoint(path: &Path, edit: impl FnOnce(&mut Value)) {
    let mut value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    edit(&mut value["state"]);
    value["sha256"] = json!(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&value["state"]).unwrap())
    ));
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn checkpoint_roundtrip_preserves_typed_inputs_empty_relations_and_revision() {
    let fixture = Fixture::new();
    let mut live = initial(&fixture);
    live.apply(&json!([{"op":"insert","predicate":"source","values":[9,"other"]}]))
        .unwrap();
    live.apply(&json!([{"op":"delete","predicate":"source","values":[9,"other"]}]))
        .unwrap();
    let inputs = live.export_inputs().unwrap();
    assert!(inputs["unused"].is_empty());
    let path = fixture.root.join("checkpoint.json");
    let receipt = live
        .save_checkpoint(&path, json!({"opaque":["batch-1",3]}))
        .unwrap();
    assert_eq!(receipt["revision"], live.revision());
    let mut restored = fixture.backend("restored");
    assert_eq!(
        restored.restore_checkpoint(&path).unwrap(),
        json!({"opaque":["batch-1",3]})
    );
    assert_eq!(restored.export_inputs().unwrap(), inputs);
    assert_eq!(restored.revision(), live.revision());
    assert_eq!(restored.program_source(), live.program_source());
    assert_eq!(restored.schemas(), live.schemas());
    assert_eq!(
        restored.query_typed("echo").unwrap(),
        live.query_typed("echo").unwrap()
    );
    assert!(live.restore_checkpoint(&path).is_err());
    let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn derived_schema_edits_replay_inputs_and_failed_replacement_preserves_live_program() {
    let fixture = Fixture::new();
    let mut live = initial(&fixture);
    let original = live.export_inputs().unwrap();
    let mut next = schemas();
    next["added"] = json!({"input":false,"fields":["int","string"]});
    live.install(
        "echo(N,S) :- source(N,S). added(N,S) :- source(N,S).",
        next.clone(),
    )
    .unwrap();
    assert_eq!(live.query_typed("added").unwrap(), original["source"]);
    next.as_object_mut().unwrap().remove("added");
    next["echo"]["fields"] = json!(["int"]);
    live.install("echo(N) :- source(N,S).", next.clone())
        .unwrap();
    assert_eq!(live.query_typed("echo").unwrap(), vec![vec![json!(-7)]]);
    let source = live.program_source().to_string();
    let revision = live.revision();
    for flag in ["reject_build", "fail_replay"] {
        fixture.flag(flag);
        assert!(live
            .install("echo(N) :- source(N,S), N < 0.", next.clone())
            .is_err());
        fixture.unflag(flag);
        assert_eq!(live.program_source(), source);
        assert_eq!(live.revision(), revision);
        assert_eq!(live.export_inputs().unwrap(), original);
        assert_eq!(live.query_typed("echo").unwrap(), vec![vec![json!(-7)]]);
    }
    next["unused"]["fields"] = json!(["int"]);
    assert!(live.install("echo(N) :- source(N,S).", next).is_err());
}

#[test]
fn invalid_mutation_has_no_partial_commit_and_lost_ack_disables_checkpoint() {
    let fixture = Fixture::new();
    let mut live = initial(&fixture);
    let revision = live.revision();
    let inputs = live.export_inputs().unwrap();
    assert!(live
        .apply(&json!([
            {"op":"insert","predicate":"source","values":[3,"new"]},
            {"op":"insert","predicate":"source","values":["bad","value"]}
        ]))
        .is_err());
    assert_eq!(live.revision(), revision);
    assert_eq!(live.export_inputs().unwrap(), inputs);
    fixture.flag("die_on_commit");
    assert!(live
        .apply(&json!([{ "op":"insert","predicate":"source","values":[3,"new"]}]))
        .is_err());
    assert_eq!(live.health(), "failed");
    assert_eq!(live.revision(), revision);
    assert!(live.export_inputs().is_err());
    assert!(live
        .save_checkpoint(&fixture.root.join("unsafe.json"), Value::Null)
        .is_err());
}

#[test]
fn corrupt_and_unsupported_checkpoints_fail_before_compilation_or_activation() {
    let fixture = Fixture::new();
    let live = initial(&fixture);
    let path = fixture.root.join("checkpoint.json");
    live.save_checkpoint(&path, Value::Null).unwrap();
    let original = fs::read(&path).unwrap();
    let mut bad: Value = serde_json::from_slice(&original).unwrap();
    bad["state"]["revision"] = json!(99);
    fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
    assert!(fixture
        .backend("bad-digest")
        .restore_checkpoint(&path)
        .unwrap_err()
        .contains("integrity"));
    let edits: Vec<Box<dyn FnOnce(&mut Value)>> = vec![
        Box::new(|state| state["format_version"] = json!(999)),
        Box::new(|state| state["program_version"] = json!(999)),
        Box::new(|state| {
            state["schemas"]["agent_intent"] = json!({"input":true,"fields":["string"]})
        }),
        Box::new(|state| {
            state["source"] = json!(format!(
                "import lemmalog_star as lemmalog_star\n{}",
                state["source"].as_str().unwrap()
            ))
        }),
        Box::new(|state| {
            state["inputs"].as_object_mut().unwrap().remove("unused");
        }),
        Box::new(|state| state["inputs"]["source"][0][0] = json!("wrong type")),
        Box::new(|state| state["source"] = json!("input relation R_wrong(f0: string)")),
        Box::new(|state| {
            let row = state["inputs"]["source"][0].clone();
            state["inputs"]["source"].as_array_mut().unwrap().push(row);
        }),
        Box::new(|state| state["schemas"]["source"]["fields"][0] = json!("float")),
    ];
    for (i, edit) in edits.into_iter().enumerate() {
        fs::write(&path, &original).unwrap();
        rewrite_checkpoint(&path, edit);
        let mut target = fixture.backend(&format!("bad-{i}"));
        assert!(target.restore_checkpoint(&path).is_err());
        assert_eq!(target.health(), "uninitialized");
        assert_eq!(target.revision(), 0);
        assert!(!fixture.root.join(format!("bad-{i}")).exists());
    }
    fs::write(&path, &original).unwrap();
    fixture.flag("fail_replay");
    let mut target = fixture.backend("failed-replay");
    assert!(target.restore_checkpoint(&path).is_err());
    assert_eq!(target.health(), "uninitialized");
    assert_eq!(target.revision(), 0);
    fixture.unflag("fail_replay");
    target.restore_checkpoint(&path).unwrap();
}

#[test]
fn checkpoint_replacement_is_complete_and_rejects_external_operation_state() {
    let fixture = Fixture::new();
    let mut live = initial(&fixture);
    let path = fixture.root.join("checkpoint.json");
    live.save_checkpoint(&path, json!(1)).unwrap();
    live.save_checkpoint(&path, json!(2)).unwrap();
    assert_eq!(
        fixture
            .backend("replaced")
            .restore_checkpoint(&path)
            .unwrap(),
        json!(2)
    );
    let mut operation = fixture.backend("operation");
    operation
        .install(
            "echo(N,S) :- source(N,S).",
            json!({
                "source":{"input":true,"fields":["int","string"]},
                "agent_intent":{"input":true,"fields":["string"]},
                "echo":{"input":false,"fields":["int","string"]}
            }),
        )
        .unwrap();
    assert!(operation
        .save_checkpoint(&path, Value::Null)
        .unwrap_err()
        .contains("Registered-operation"));
    assert_eq!(
        fixture
            .backend("preserved")
            .restore_checkpoint(&path)
            .unwrap(),
        json!(2)
    );
    fixture.flag("malformed_query");
    assert!(live.query_typed("echo").is_err());
}

#[test]
fn failed_publication_does_not_replace_target_or_leave_partial_file() {
    let fixture = Fixture::new();
    let live = initial(&fixture);
    let target = fixture.root.join("existing-directory");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("preserve"), "original").unwrap();
    assert!(live.save_checkpoint(&target, Value::Null).is_err());
    assert_eq!(
        fs::read_to_string(target.join("preserve")).unwrap(),
        "original"
    );
    assert!(!fs::read_dir(&fixture.root).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp")));
}
