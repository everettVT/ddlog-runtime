#![cfg(unix)]
//! Admission/settlement contracts using simulated transport, not native evaluation.
use ddlog_runtime::{AgentProgram, Backend, Operation};
use serde_json::json;
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
            "ddlog-requests-test-{}-{}",
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

fn install(f: &Fixture) -> (AgentProgram, Backend) {
    let mut backend = f.backend("live");
    let (agent, _) = AgentProgram::install(
        &mut backend,
        "infer",
        Operation {
            version: "v1".into(),
            description: "fixture".into(),
        },
        "answer(E,R,O) :- agent_result(E,R,O).",
        json!({"answer":{"input":false,"fields":["string","int","string"]}}),
    )
    .unwrap();
    (agent, backend)
}
#[test]
fn completion_requires_claim_and_preserves_current_and_historical_results() {
    let f = Fixture::new();
    let (mut agent, mut backend) = install(&f);
    let old = agent.submit(&mut backend, "entity", 1, "first").unwrap()["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(agent
        .complete(&mut backend, &old, "answer")
        .unwrap_err()
        .contains("claimed"));
    agent.claim(&mut backend, &old).unwrap();
    assert!(agent.claim(&mut backend, &old).is_err());
    let new = agent.submit(&mut backend, "entity", 2, "second").unwrap()["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        agent.complete(&mut backend, &old, "old result").unwrap()["fresh"],
        false
    );
    let revision = backend.revision();
    assert_eq!(
        agent.complete(&mut backend, &old, "old result").unwrap()["duplicate"],
        true
    );
    assert_eq!(backend.revision(), revision);
    assert!(agent
        .complete(&mut backend, &old, "different")
        .unwrap_err()
        .contains("Conflicting"));
    agent.claim(&mut backend, &new).unwrap();
    let done = agent.complete(&mut backend, &new, "new result").unwrap();
    assert_eq!(done["fresh"], true);
    assert_eq!(done["duplicate"], false);
    assert!(agent.submit(&mut backend, "entity", 1, "first").is_err());
    assert!(agent.submit(&mut backend, "entity", 2, "changed").is_err());
    assert!(agent.complete(&mut backend, "missing", "answer").is_err());
}
#[test]
fn lost_completion_ack_keeps_request_unsettled_and_disables_runtime_reuse() {
    let f = Fixture::new();
    let (mut agent, mut backend) = install(&f);
    let id = agent.submit(&mut backend, "entity", 1, "input").unwrap()["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    agent.claim(&mut backend, &id).unwrap();
    f.flag("die_on_commit");
    assert!(agent.complete(&mut backend, &id, "result").is_err());
    assert_eq!(backend.health(), "failed");
    assert_eq!(agent.status()["requests"][0]["status"], "claimed");
    f.unflag("die_on_commit");
    assert!(agent.complete(&mut backend, &id, "result").is_err());
    assert_eq!(agent.status()["requests"][0]["status"], "claimed");
}

#[test]
fn signed_integer_boundaries_and_invalid_tail_preserve_complete_input_state() {
    let f = Fixture::new();
    let mut backend = f.backend("typed");
    backend
        .install(
            "echo(N,S) :- source(N,S).",
            json!({
                "source":{"input":true,"fields":["int","string"]},
                "echo":{"input":false,"fields":["int","string"]}
            }),
        )
        .unwrap();
    backend
        .apply(&json!([
            {"op":"insert","predicate":"source","values":[i64::MIN,"low"]},
            {"op":"insert","predicate":"source","values":[i64::MAX,"high"]}
        ]))
        .unwrap();
    let before = serde_json::to_value(backend.export_inputs().unwrap()).unwrap();
    let revision = backend.revision();
    for invalid in [json!(u64::MAX), json!(1.5), json!(true), json!("1")] {
        assert!(backend
            .apply(&json!([
                {"op":"delete","predicate":"source","values":[i64::MIN,"low"]},
                {"op":"insert","predicate":"source","values":[invalid,"invalid"]}
            ]))
            .is_err());
        assert_eq!(backend.revision(), revision);
        assert_eq!(
            serde_json::to_value(backend.export_inputs().unwrap()).unwrap(),
            before
        );
    }
}
