#![cfg(unix)]
//! Drives the `ddlog-worlds` binary over its newline-delimited stdio protocol
//! with the simulated build driver; no native DDlog evaluation is claimed.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Owner {
    root: PathBuf,
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}
impl Owner {
    fn spawn() -> Self {
        let root = std::env::temp_dir().join(format!(
            "worlds-stdio-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let template =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/memory_fake_runtime.py");
        std::fs::write(root.join("build.py"), format!("#!/usr/bin/env python3\nimport sys\nfrom pathlib import Path\nsource=Path({}).read_text().replace('__CONTROL__',{})\nPath(sys.argv[2]).write_text(source)\nPath(sys.argv[2]).chmod(0o700)\n", json!(template), json!(serde_json::to_string(&root).unwrap()))).unwrap();
        std::fs::set_permissions(
            root.join("build.py"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_ddlog-worlds"))
            .arg(root.join("registry"))
            .arg(root.join("worlds"))
            .arg(root.join("build.py"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            root,
        }
    }
    fn raw(&mut self, line: &str) -> Value {
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{line}").unwrap();
        input.flush().unwrap();
        let mut reply = String::new();
        assert!(
            self.output.read_line(&mut reply).unwrap() > 0,
            "owner closed its stdout"
        );
        serde_json::from_str(&reply).unwrap()
    }
    fn call(&mut self, operation: &str, args: Value) -> Value {
        let reply = self.raw(&json!({"operation":operation,"args":args}).to_string());
        assert_eq!(reply["ok"], true, "{operation} failed: {reply}");
        reply["result"].clone()
    }
    fn fail(&mut self, operation: &str, args: Value) -> String {
        let reply = self.raw(&json!({"operation":operation,"args":args}).to_string());
        assert_eq!(
            reply["ok"], false,
            "{operation} unexpectedly succeeded: {reply}"
        );
        reply["error"].as_str().unwrap().to_string()
    }
    fn wait(&mut self, id: &str) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let status = self.call("status", json!({"id":id}));
            if status["state"] != "starting" {
                return status;
            }
            assert!(std::time::Instant::now() < deadline, "{status}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn stdio_protocol_covers_every_control_plane_verb() {
    let mut owner = Owner::spawn();
    let info = owner.call("runtime_info", json!({}));
    assert_eq!(info["schema_version"], 1);
    assert_eq!(info["crate_version"], env!("CARGO_PKG_VERSION"));
    assert!(info["dirty"].is_boolean());
    assert_eq!(owner.raw("not json")["ok"], false);
    assert!(owner
        .fail("nonsense", json!({}))
        .contains("Unknown control-plane operation"));
    assert_eq!(owner.call("inventory", json!({}))["worlds"], json!([]));
    assert_eq!(owner.call("inventory", Value::Null)["worlds"], json!([]));
    let libraries = owner.call("libraries", json!({}));
    assert_eq!(libraries["libraries"][0]["id"], "unassigned");
    let definition = json!({"rules":"echo(N,S) :- source(N,S).","schemas":{"source":{"input":true,"fields":["int","string"]},"echo":{"input":false,"fields":["int","string"]}}});
    assert!(owner
        .fail("register", json!({"definition":definition}))
        .contains("name"));
    let library = owner.call(
        "library_create",
        json!({"name":"Examples","repository":"https://example.invalid/e","revision":"main"}),
    );
    let record = owner.call("register", json!({"name":"Echo","description":"copies","library_id":library["id"],"definition":definition}));
    assert_eq!(record["name"], "Echo");
    assert_eq!(record["library_id"], library["id"]);
    let pin = json!({"processor_id":record["processor_id"],"version":record["version"]});
    let definitions = owner.call("definitions", json!({}));
    assert_eq!(definitions["processors"][0]["name"], "Echo");
    assert_eq!(definitions["processors"][0]["kind"], "program");
    assert_eq!(definitions["processors"][0]["current"], true);
    assert_eq!(
        owner.call("definition", pin.clone())["processor_id"],
        record["processor_id"]
    );
    let libraries = owner.call("libraries", json!({}));
    let filed = libraries["libraries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == library["id"])
        .unwrap();
    assert_eq!(filed["processors"][0]["name"], "Echo");
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/upstream/registry");
    let imported = owner.call(
        "import",
        json!({"source_registry":fixture,"library_id":library["id"],"dry_run":true}),
    );
    assert_eq!(imported["imported"].as_array().unwrap().len(), 5);
    assert_eq!(imported["errors"], json!([]));
    assert_eq!(
        owner.call("definitions", json!({}))["processors"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let imported = owner.call("import", json!({"source_registry":fixture}));
    assert_eq!(imported["imported"].as_array().unwrap().len(), 5);
    assert_eq!(
        owner.call("definitions", json!({}))["processors"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    let created = owner.call("create", json!({"label":"Echo world","processor":pin}));
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["state"], "created");
    assert_eq!(created["persistence"]["status"], "not_configured");
    assert_eq!(owner.call("start", json!({"id":id}))["state"], "starting");
    let running = owner.wait(&id);
    assert_eq!(running["state"], "running");
    assert_eq!(running["instance"]["revision"], 1);
    assert!(running["started_at_unix_ms"].is_number());
    let applied = owner.call("execute", json!({"id":id,"operation":"apply_changes","args":{"changes":[{"op":"insert","predicate":"source","values":[7,"seven"]}]}}));
    assert_eq!(applied["revision"], 2);
    let relations = owner.call(
        "execute",
        json!({"id":id,"operation":"relations","args":{}}),
    );
    assert_eq!(
        relations["relations"][1],
        json!({"name":"echo","input":false,"fields":["int","string"],"count":1})
    );
    let rows = owner.call(
        "execute",
        json!({"id":id,"operation":"query_rows","args":{"predicate":"echo"}}),
    );
    assert_eq!(rows["rows"], json!([[7, "seven"]]));
    assert_eq!(rows["complete"], true);
    let source = owner.call(
        "execute",
        json!({"id":id,"operation":"program_source","args":{}}),
    );
    assert!(source["source"].as_str().unwrap().contains("R_echo"));
    assert!(owner
        .fail(
            "execute",
            json!({"id":id,"operation":"processor_install","args":{}})
        )
        .contains("registry"));
    let summary = owner.call(
        "inventory",
        json!({"summary":true,"processor_id":pin["processor_id"]}),
    );
    assert_eq!(summary["worlds"].as_array().unwrap().len(), 1);
    assert!(summary["worlds"][0].get("inspection").is_none());
    assert!(summary["worlds"][0].get("instance").is_none());
    assert_eq!(owner.call("stop", json!({"id":id}))["state"], "stopped");
    let scenarios = json!([{"name":"one","description":"","changes":[{"op":"insert","predicate":"source","values":[1,"a"]}],"expect":{"echo":[[1,"a"]]}}]);
    assert!(owner.fail("scenarios_set", json!({"processor_id":pin["processor_id"],"version":pin["version"],"scenarios":[{"name":"bad","changes":[{"op":"insert","predicate":"echo","values":[1,"a"]}],"expect":{}}]})).contains("public input"));
    assert_eq!(owner.call("scenarios_set", json!({"processor_id":pin["processor_id"],"version":pin["version"],"scenarios":scenarios}))["scenarios"], scenarios);
    assert_eq!(
        owner.call("scenarios_get", pin.clone())["scenarios"],
        scenarios
    );
    assert_eq!(
        owner.call("capture_get", pin.clone())["state"],
        "missing",
        "the simulated runtime writes no native capture"
    );
    assert!(owner
        .fail(
            "capture_get",
            json!({"processor_id":"processor_missing","version":pin["version"]})
        )
        .contains("processor"));
    let test = owner.call(
        "test",
        json!({"processor_id":pin["processor_id"],"version":pin["version"]}),
    );
    assert_eq!(test["definition"]["purpose"], "test");
    assert_eq!(test["definition"]["label"], "Test · Echo");
    assert_eq!(test["test"]["phase"], "building");
    let test_id = test["id"].as_str().unwrap().to_string();
    let finished = owner.wait(&test_id);
    assert_eq!(finished["state"], "stopped");
    assert_eq!(finished["test"]["phase"], "done");
    assert_eq!(finished["test"]["passed"], true);
    assert_eq!(finished["test"]["results"][0]["revision"], 2);
    assert_eq!(
        owner.call("inventory", json!({})).as_object().unwrap()["worlds"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    drop(owner.input.take());
    let status = owner.child.wait().unwrap();
    assert!(status.success());
}
