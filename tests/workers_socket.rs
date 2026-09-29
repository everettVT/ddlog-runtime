#![cfg(unix)]
//! The identical attached worker flow runs against fake transport and real DDLog.
use ddlog_runtime::registry::{ProcessorDefinition, ProcessorRegistry};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::{fs::PermissionsExt, net::UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Owner {
    child: Child,
    endpoint: PathBuf,
}
impl Owner {
    fn launch(root: &Path, driver: &Path) -> Self {
        let endpoint = root.join("owner.json");
        let child = Command::new(env!("CARGO_BIN_EXE_ddlog-worlds"))
            .arg(root.join("registry"))
            .arg(root.join("worlds"))
            .arg(driver)
            .arg("--listen")
            .arg(&endpoint)
            .arg("--worker-profiles")
            .arg(root.join("profiles.json"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut owner = Self { child, endpoint };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !owner.endpoint.exists() {
            assert!(
                owner.child.try_wait().unwrap().is_none(),
                "owner exited before publication"
            );
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        owner
    }
    fn call(&self, operation: &str, args: Value) -> Value {
        call(&self.endpoint, operation, args)
    }
    fn status(&self, id: &str) -> Value {
        self.call("status", json!({"id":id}))
    }
    fn wait(&self, id: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let status = self.status(id);
            if done(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "{status}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn worker(&self, id: &str, generation: u64, key: &str, mode: &str) -> Value {
        self.call("worker_start",json!({"id":id,"expected_generation":generation,"profile":"fixture","key":key,"payload":{"mode":mode}}))
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("isolated owner shutdown exceeded deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
fn call(endpoint: &Path, operation: &str, args: Value) -> Value {
    let descriptor: Value = serde_json::from_slice(&fs::read(endpoint).unwrap()).unwrap();
    let mut stream = UnixStream::connect(descriptor["socket"].as_str().unwrap()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut bytes=serde_json::to_vec(&json!({"schema_version":1,"request_id":"fixture-client","owner_incarnation":descriptor["owner_incarnation"],"operation":operation,"args":args})).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], true, "{response}");
    response["result"].clone()
}
fn file(path: &Path, owner: &Owner, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        let status = owner.status(id);
        assert!(
            !status["workers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["state"] == "failed"),
            "{status}"
        );
        assert!(
            Instant::now() < deadline,
            "waiting for {}: {status}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "owned test pid {pid} survived");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn exercise(native: bool) {
    let root =
        std::env::temp_dir().join(format!("wks-{}-{}", std::process::id(), u8::from(native)));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let registry = ProcessorRegistry::open(root.join("registry")).unwrap();
    let definition:ProcessorDefinition=serde_json::from_value(json!({"rules":"echo(N,S) :- source(N,S).","schemas":{"source":{"input":true,"fields":["int","string"]},"echo":{"input":false,"fields":["int","string"]}}})).unwrap();
    let record = registry.create(definition, None).unwrap();
    let pin = json!({"processor_id":record.processor_id,"version":record.version});
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::write(root.join("profiles.json"),serde_json::to_vec(&json!({"schema_version":1,"profiles":{"fixture":{
        "argv":["/usr/bin/env","python3",manifest.join("tests/fixtures/managed_worker.py")],
        "env":{"FIXTURE_ROOT":root},"allowed_processors":[pin],"max_concurrent_workers":2,"timeout_ms":30000
    }}})).unwrap()).unwrap();
    fs::set_permissions(
        root.join("profiles.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let driver = if native {
        PathBuf::from(
            std::env::var_os("DDLOG_RUNTIME_NATIVE_BUILD").expect("Configure native build driver"),
        )
    } else {
        let path = root.join("build.py");
        fs::write(&path,format!("#!/usr/bin/env python3\nimport sys\nfrom pathlib import Path\nsource=Path({}).read_text().replace('__CONTROL__',{})\nPath(sys.argv[2]).write_text(source)\nPath(sys.argv[2]).chmod(0o700)\n",json!(manifest.join("tests/fixtures/memory_fake_runtime.py")),json!(serde_json::to_string(&root).unwrap()))).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let owner = Owner::launch(&root, &driver);
    let id=owner.call("create",json!({"label":"bounded worker fixture","processor":pin,"purpose":"instance","scenarios":[]}))["id"].as_str().unwrap().to_string();
    owner.call("start", json!({"id":id}));
    let started = owner.wait(&id, |s| s["state"] != "starting");
    assert_eq!(started["state"], "running", "{started}");
    let worker = owner.worker(&id, 1, "attached", "attached");
    // Every call has already closed its socket. No HTTP/client process owns this worker.
    file(&root.join("attached.reserved.json"), &owner, &id);
    let reserved: Value =
        serde_json::from_slice(&fs::read(root.join("attached.reserved.json")).unwrap()).unwrap();
    assert_eq!(reserved["effect_authorized"], true);
    assert_eq!(reserved["applied_revision"], 2);
    assert_eq!(owner.status(&id)["workers"][0]["state"], "running");
    assert_eq!(
        owner.status(&id)["resources"]["pid"],
        started["resources"]["pid"]
    );
    let historical = owner.call(
        "admission_status",
        json!({"id":id,"generation":1,"admission_key":"attached.reserve"}),
    );
    assert_eq!(historical["effect_authorized"], false);
    fs::write(root.join("attached.release"), "").unwrap();
    let finished = owner.wait(&id, |s| s["workers"][0]["state"] != "running");
    assert_eq!(finished["workers"][0]["state"], "completed", "{finished}");
    let settled: Value =
        serde_json::from_slice(&fs::read(root.join("attached.settled.json")).unwrap()).unwrap();
    assert_eq!(settled["applied_revision"], 3);
    assert_eq!(settled["state"], "durable");
    let evidence=owner.call("read_batch",json!({"id":id,"expected_generation":1,"expected_revision":3,"queries":[{"predicate":"source"},{"predicate":"echo"}]}));
    assert_eq!(evidence["results"][0]["rows"], json!([[20, "settled"]]));
    assert_eq!(evidence["results"][1]["rows"], json!([[20, "settled"]]));
    // Two independently attached writers race the same evidence revision.
    let requests:Vec<_>=(0..2).map(|n|{let endpoint=owner.endpoint.clone();let id=id.clone();std::thread::spawn(move||call(&endpoint,"admit_inputs",json!({"id":id,"expected_generation":1,"expected_revision":3,"admission_key":format!("contender-{n}"),"changes":[]})))}).collect();
    let results: Vec<_> = requests.into_iter().map(|r| r.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|r| r["state"] == "durable").count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| r["state"] == "not_applied")
            .count(),
        1
    );
    let duplicate = owner.worker(&id, 1, "attached", "attached");
    assert_eq!(duplicate["worker_id"], worker["worker_id"]);
    assert_eq!(duplicate["replayed"], true);
    owner.call("stop", json!({"id":id}));
    drop(owner);
    let owner = Owner::launch(&root, &driver);
    assert_eq!(owner.status(&id)["workers"][0]["state"], "completed");
    owner.call("restore", json!({"id":id,"receipt":reserved["receipt"]}));
    let restored = owner.wait(&id, |s| s["state"] != "starting");
    assert_eq!(restored["state"], "running", "{restored}");
    assert_eq!(restored["generation"], 2);
    assert_eq!(restored["revision"], 2);
    let duplicate=owner.call("admit_inputs",json!({"id":id,"expected_generation":2,"expected_revision":2,"admission_key":"restored-duplicate","changes":[],"effect":{"key":"attached","phase":"reserve"}}));
    assert_eq!(duplicate["state"], "not_applied");
    assert_eq!(duplicate["effect_authorized"], false);
    let old=owner.call("admit_inputs",json!({"id":id,"expected_generation":2,"expected_revision":2,"admission_key":"late","worker_id":worker["worker_id"],"changes":[]}));
    assert_eq!(old["state"], "not_applied");
    let evidence=owner.call("read_batch",json!({"id":id,"expected_generation":2,"expected_revision":2,"queries":[{"predicate":"echo"}]}));
    assert_eq!(evidence["results"][0]["rows"], json!([[10, "reserved"]]));
    // Real owner SIGTERM must kill worker descendants as well as the native process.
    let dying = owner.worker(&id, 2, "owner-exit", "descendant");
    file(&root.join("owner-exit.child"), &owner, &id);
    let descendant = fs::read_to_string(root.join("owner-exit.child"))
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let native_pid = owner.status(&id)["resources"]["pid"].as_u64().unwrap() as i32;
    drop(owner);
    gone(dying["pid"].as_u64().unwrap() as i32);
    gone(descendant);
    gone(native_pid);
    if native {
        if let Some(path) = std::env::var_os("DDLOG_WORKER_EVIDENCE") {
            fs::write(path,serde_json::to_vec_pretty(&json!({"schema_version":1,"native":true,"reserved":reserved,"settled":settled,"restored":restored["persistence"],"competing_admissions":results,"worker":finished["workers"][0],"cleanup":"owner, worker descendant and native exited"})).unwrap()).unwrap();
        }
    }
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn attached_worker_admission_replay_restore_and_owner_shutdown() {
    exercise(false);
}
#[test]
#[ignore = "requires operator-configured DDLOG_RUNTIME_NATIVE_BUILD toolchain"]
fn native_attached_worker_admission_replay_restore_and_owner_shutdown() {
    exercise(true);
}
