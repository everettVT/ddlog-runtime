#![cfg(all(unix, feature = "iceberg"))]
//! Real SQLite/Parquet/catalog IO. Native DDLog proof is a separate opt-in test.
use ddlog_runtime::{
    registry::{ProcessorDefinition, ProcessorReference},
    worlds::{WorldDefinition, WorldManager},
};
use iceberg::{Catalog, CatalogBuilder, TableIdent};
use iceberg_catalog_sql::{SqlBindStyle, SqlCatalogBuilder};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    driver: PathBuf,
}
impl Fixture {
    fn new(native: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "managed-iceberg-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir(&root).unwrap();
        let driver = if native {
            std::env::var_os("DDLOG_RUNTIME_NATIVE_BUILD")
                .expect("native build driver")
                .into()
        } else {
            let template =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/memory_fake_runtime.py");
            let driver = root.join("build.py");
            fs::write(&driver,format!("#!/usr/bin/env python3\nimport sys\nfrom pathlib import Path\nsource=Path({}).read_text().replace('__CONTROL__',{})\nPath(sys.argv[2]).write_text(source)\nPath(sys.argv[2]).chmod(0o700)\n",json!(template),json!(serde_json::to_string(&root).unwrap()))).unwrap();
            fs::set_permissions(&driver, fs::Permissions::from_mode(0o700)).unwrap();
            driver
        };
        let f = Self { root, driver };
        f.profile(30000);
        f
    }
    fn storage(&self) -> PathBuf {
        self.root.join("local store")
    }
    fn profile(&self, timeout: u64) {
        fs::write(self.root.join("profile.json"),serde_json::to_vec(&json!({"schema_version":1,"profile":{"name":"local","root":self.storage(),"timeout_ms":timeout}})).unwrap()).unwrap();
        fs::set_permissions(
            self.root.join("profile.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    fn manager(&self) -> WorldManager {
        let mut manager = WorldManager::new(
            self.root.join("registry"),
            self.root.join("worlds"),
            self.driver.clone(),
        )
        .unwrap();
        manager
            .configure_storage(&self.root.join("profile.json"))
            .unwrap();
        manager
    }
    fn record(&self, receipt: &Value) -> PathBuf {
        self.root
            .join("worlds")
            .join(receipt["origin"]["world_id"].as_str().unwrap())
            .join("checkpoints")
            .join(receipt["receipt_id"].as_str().unwrap())
            .join("publication.json")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
fn make(manager: &mut WorldManager, composition: bool) -> String {
    let definition:ProcessorDefinition=serde_json::from_value(json!({"rules":"echo(N,S) :- source(N,S).","schemas":{"source":{"input":true,"fields":["int","string"]},"unused":{"input":true,"fields":["string"]},"echo":{"input":false,"fields":["int","string"]}},"interface":{"inputs":["source","unused"],"outputs":["echo"]}})).unwrap();
    let leaf = manager
        .registry()
        .unwrap()
        .create(definition, None)
        .unwrap();
    let leaf = ProcessorReference {
        processor_id: leaf.processor_id,
        version: leaf.version,
    };
    let pin = if composition {
        let definition=serde_json::from_value(json!({"composition":{"nodes":{"child":leaf},"bindings":[],
            "inputs":{"source":{"fields":["int","string"],"targets":[{"node":"child","relation":"source"}]},"unused":{"fields":["string"],"targets":[{"node":"child","relation":"unused"}]}},"outputs":{"echo":{"node":"child","relation":"echo"}}}})).unwrap();
        let record = manager
            .registry()
            .unwrap()
            .create(definition, None)
            .unwrap();
        ProcessorReference {
            processor_id: record.processor_id,
            version: record.version,
        }
    } else {
        leaf
    };
    manager
        .create(WorldDefinition {
            label: "Iceberg recovery".into(),
            processor: pin,
            purpose: "instance".into(),
            scenarios: vec![],
        })
        .unwrap()
}
fn change(m: &mut WorldManager, id: &str, op: &str, values: Value) {
    m.execute(
        id,
        "apply_changes",
        &json!({"changes":[{"op":op,"predicate":"source","values":values}]}),
    )
    .unwrap();
}
fn rows(m: &mut WorldManager, id: &str, name: &str) -> Value {
    m.execute(id, "query_rows", &json!({"predicate":name}))
        .unwrap()["rows"]
        .clone()
}
fn stage(m: &mut WorldManager, id: &str, revision: u64) -> Value {
    m.checkpoint_stage(
        serde_json::from_value(
            json!({"id":id,"expected_generation":1,"expected_revision":revision,"profile":"local"}),
        )
        .unwrap(),
    )
    .unwrap()
}
fn publication(m: &mut WorldManager, ticket: &Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(35);
    loop {
        let p = m
            .checkpoint_status(
                serde_json::from_value(
                    json!({"id":ticket["id"],"receipt_id":ticket["receipt_id"]}),
                )
                .unwrap(),
            )
            .unwrap();
        if !matches!(p["status"].as_str().unwrap(), "staging" | "publishing") {
            return p;
        }
        assert!(Instant::now() < deadline, "{p}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn publish(m: &mut WorldManager, id: &str, receipt: &Value) -> Value {
    let ticket = m
        .checkpoint_publish(serde_json::from_value(json!({"id":id,"receipt":receipt})).unwrap())
        .unwrap();
    publication(m, &ticket)
}
fn running(m: &mut WorldManager, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let status = m.status(id).unwrap();
        if status["state"] != "starting" {
            return status;
        }
        assert!(Instant::now() < deadline, "{status}");
        std::thread::sleep(Duration::from_millis(30));
    }
}
fn snapshots(f: &Fixture) -> Vec<i64> {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let catalog = SqlCatalogBuilder::default()
            .uri(format!(
                "sqlite://{}?mode=rw",
                f.storage().join("catalog.sqlite").display()
            ))
            .warehouse_location(format!("file://{}/warehouse", f.storage().display()))
            .sql_bind_style(SqlBindStyle::QMark)
            .with_storage_factory(Arc::new(iceberg::io::LocalFsStorageFactory))
            .load("managed-checkpoints", HashMap::new())
            .await
            .unwrap();
        let table = catalog
            .load_table(&TableIdent::from_strs(["runtime", "checkpoints"]).unwrap())
            .await
            .unwrap();
        table
            .metadata()
            .snapshots()
            .map(|s| s.snapshot_id())
            .collect()
    })
}
fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn write(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}
fn exercise(native: bool) {
    let f = Fixture::new(native);
    let mut m = f.manager();
    let id = make(&mut m, native);
    let second = make(&mut m, false);
    m.start(&id).unwrap();
    m.start(&second).unwrap();
    let admission=m.admit_inputs(serde_json::from_value(json!({"id":id,"expected_generation":1,"expected_revision":1,"admission_key":"before-iceberg",
        "changes":[{"op":"insert","predicate":"source","values":[-7,"saved\n\"é"]}],"effect":{"key":"frozen-effect","phase":"reserve"}})).unwrap()).unwrap();
    assert_eq!(admission["state"], "durable");
    let origin = m.status(&id).unwrap();
    let json_receipt = m.checkpoint(&id).unwrap();
    assert!(json_receipt.get("storage").is_none());
    let ticket = stage(&mut m, &id, 2);
    let staged = publication(&mut m, &ticket);
    assert_eq!(staged["status"], "staged", "{staged}");
    let receipt = &staged["receipt"];
    assert_eq!(receipt["storage"]["snapshot_id"], Value::Null);
    assert_eq!(receipt["published_at_unix_ms"], Value::Null);
    assert!(snapshots(&f).is_empty());
    assert_ne!(
        receipt["checkpoint_sha256"],
        receipt["storage"]["envelope_sha256"]
    );
    assert_eq!(receipt["origin"]["revision"], 2);
    if native {
        assert!(!receipt["program"]["dependencies"]
            .as_object()
            .unwrap()
            .is_empty());
    }
    change(&mut m, &id, "insert", json!([9, "unpublished"]));
    let staged_record = read(&f.record(receipt));
    // Block real catalog IO while another native/simulated world stays usable.
    let lock = CatalogLock::new(&f);
    let pending = m
        .checkpoint_publish(serde_json::from_value(json!({"id":id,"receipt":receipt})).unwrap())
        .unwrap();
    let before = Instant::now();
    assert_eq!(m.status(&second).unwrap()["state"], "running");
    let status_ms = before.elapsed().as_secs_f64() * 1000.0;
    let before = Instant::now();
    assert_eq!(rows(&mut m, &second, "echo"), json!([]));
    let query_ms = before.elapsed().as_secs_f64() * 1000.0;
    assert!(
        status_ms < 1000.0 && query_ms < 1000.0,
        "status={status_ms}, query={query_ms}"
    );
    drop(lock);
    let published = publication(&mut m, &pending);
    assert_eq!(published["status"], "published", "{published}");
    let snapshot = published["receipt"]["storage"]["snapshot_id"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    assert_eq!(snapshots(&f), vec![snapshot]);
    assert_eq!(
        publish(&mut m, &id, &published["receipt"])["receipt"],
        published["receipt"]
    );
    // Model a lost final readback/manifest write after a real catalog commit.
    // The immutable staged receipt remains sufficient for explicit reconciliation.
    let mut uncertain = staged_record;
    uncertain["status"] = json!("publishing");
    write(&f.record(receipt), &uncertain);
    m.stop(&id).unwrap();
    m.stop(&second).unwrap();
    drop(m);
    let mut m = f.manager();
    let recovered = publication(&mut m, &ticket);
    assert_eq!(recovered["status"], "uncertain");
    assert_eq!(recovered["receipt"], *receipt);
    let published = publish(&mut m, &id, receipt);
    assert_eq!(published["status"], "published", "{published}");
    assert_eq!(snapshots(&f), vec![snapshot]);
    assert_eq!(
        published["receipt"]["storage"]["snapshot_id"],
        snapshot.to_string()
    );
    for (pointer, value) in [
        ("/storage/object_uri", json!("file:///etc/passwd")),
        ("/storage/snapshot_id", json!(1)),
        ("/storage/snapshot_id", json!("1")),
        ("/checkpoint_sha256", json!("0".repeat(64))),
    ] {
        let mut altered = published["receipt"].clone();
        *altered.pointer_mut(pointer).unwrap() = value;
        assert!(m.restore_async(&id, &altered).is_err());
    }
    assert!(
        m.restore_async(&id, receipt).is_err(),
        "staged is not a restore receipt"
    );
    m.restore_async(&id, &published["receipt"]).unwrap();
    let restored = running(&mut m, &id);
    assert_eq!(restored["state"], "running", "{restored}");
    assert_eq!(restored["generation"], 2);
    let duplicate=m.admit_inputs(serde_json::from_value(json!({"id":id,"expected_generation":2,"expected_revision":2,"admission_key":"late-retry","changes":[],"effect":{"key":"frozen-effect","phase":"reserve"}})).unwrap()).unwrap();
    assert_eq!(duplicate["state"], "not_applied");
    assert_eq!(duplicate["effect_authorized"], false);

    assert_eq!(restored["revision"], 2);
    assert_ne!(restored["resources"]["pid"], origin["resources"]["pid"]);
    assert_eq!(
        restored["persistence"]["restored_from"],
        published["receipt"]
    );
    assert_eq!(restored["persistence"]["persisted_revision"], Value::Null);
    assert_eq!(restored["persistence"]["iceberg"]["published_revision"], 2);
    assert_eq!(rows(&mut m, &id, "source"), json!([[-7, "saved\n\"é"]]));
    assert_eq!(rows(&mut m, &id, "unused"), json!([]));
    if native {
        assert_eq!(rows(&mut m, &id, "echo"), json!([[-7, "saved\n\"é"]]));
    }
    change(&mut m, &id, "delete", json!([-7, "saved\n\"é"]));
    change(&mut m, &id, "insert", json!([8, "after restore"]));
    assert_eq!(rows(&mut m, &id, "source"), json!([[8, "after restore"]]));
    if native {
        assert_eq!(rows(&mut m, &id, "echo"), json!([[8, "after restore"]]));
    }
    if native {
        // Capture ingestion has its own 100 ms tailer, independent of native
        // transaction acknowledgment. Require eventual capture, not a race.
        let deadline = Instant::now() + Duration::from_secs(5);
        let captured = loop {
            let status = m.status(&id).unwrap();
            if status["inspection"]["state"] == "available"
                && status["inspection"]["activity"]["totals"]["timely"]
                    .as_u64()
                    .unwrap_or(0)
                    > 0
            {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "Capture did not arrive: {}",
                status["inspection"]
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        if let Some(path) = std::env::var_os("DDLOG_ICEBERG_EVIDENCE") {
            write(
                Path::new(&path),
                &json!({"schema_version":1,"native":true,"staged":staged,"published":published,"restore":restored,"capture_state":captured["inspection"]["state"],"capture_generation":captured["generation"],"catalog_snapshot_id":snapshot.to_string(),"stage_catalog_invisible":true,"ambiguous_record_reconciled":true,"unpublished_mutation_excluded":true,"update_retraction":true,"public_composition":true,"provider_calls":0,"blocked_catalog_status_ms":status_ms,"blocked_catalog_query_ms":query_ms}),
            );
        }
    }
    m.stop(&id).unwrap();
    let object = f.storage().join("warehouse/objects").join(format!(
        "{}.parquet",
        receipt["receipt_id"].as_str().unwrap()
    ));
    fs::write(&object, "tampered").unwrap();
    m.restore_async(&id, &published["receipt"]).unwrap();
    let failed = running(&mut m, &id);
    assert_eq!(failed["state"], "failed");
    assert!(!f.root.join("worlds").join(&id).join("3/build-1").exists());
}
#[test]
fn managed_local_iceberg_recovery_and_exact_receipts() {
    exercise(false);
}
#[test]
#[ignore = "requires DDLOG_RUNTIME_NATIVE_BUILD; fresh isolated toolchain execution"]
fn native_managed_iceberg_composition_recovery() {
    exercise(true);
}

/// A real exclusive SQLite lock forces a pending storage job, without a fake
/// catalog or production fault switches. Only this fresh fixture DB is touched.
struct CatalogLock {
    release: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl CatalogLock {
    fn new(f: &Fixture) -> Self {
        let path = f.storage().join("catalog.sqlite");
        let (ready, wait) = std::sync::mpsc::channel();
        let (release, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            use sqlx::{Connection, Executor};
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let mut conn = sqlx::SqliteConnection::connect_with(
                    &sqlx::sqlite::SqliteConnectOptions::new().filename(path),
                )
                .await
                .unwrap();
                conn.execute("BEGIN EXCLUSIVE").await.unwrap();
                ready.send(()).unwrap();
                rx.recv().unwrap();
                conn.execute("ROLLBACK").await.unwrap();
                conn.close().await.unwrap();
            });
        });
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        Self {
            release: Some(release),
            thread: Some(thread),
        }
    }
}
impl Drop for CatalogLock {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}
#[test]
fn storage_deadline_and_stop_preserve_responsiveness_and_exact_retry() {
    let f = Fixture::new(false);
    f.profile(500);
    let mut m = f.manager();
    let id = make(&mut m, false);
    let other = make(&mut m, false);
    m.start(&id).unwrap();
    m.start(&other).unwrap();
    let ticket = stage(&mut m, &id, 1);
    let staged = publication(&mut m, &ticket);
    assert_eq!(staged["status"], "staged", "{staged}");
    let lock = CatalogLock::new(&f);
    let before = Instant::now();
    let publish = m
        .checkpoint_publish(
            serde_json::from_value(json!({"id":id,"receipt":staged["receipt"]})).unwrap(),
        )
        .unwrap();
    assert!(before.elapsed() < Duration::from_secs(1));
    let before = Instant::now();
    assert_eq!(m.status(&other).unwrap()["state"], "running");
    assert!(before.elapsed() < Duration::from_secs(1));
    assert_eq!(rows(&mut m, &other, "source"), json!([]));
    let uncertain = publication(&mut m, &publish);
    assert_eq!(uncertain["status"], "uncertain");
    assert_eq!(uncertain["receipt"], staged["receipt"]);
    assert!(uncertain["error"].as_str().unwrap().contains("deadline"));
    drop(lock);
    let published = publish_receipt(&mut m, &id, &staged["receipt"]);
    assert_eq!(published["status"], "published", "{published}");
    let target = m
        .create(WorldDefinition {
            label: "Pending publication restore".into(),
            processor: serde_json::from_value(published["receipt"]["program"]["processor"].clone())
                .unwrap(),
            purpose: "instance".into(),
            scenarios: vec![],
        })
        .unwrap();
    let record = read(&f.record(&published["receipt"]));
    let lock = CatalogLock::new(&f);
    let pending = m
        .checkpoint_publish(
            serde_json::from_value(json!({"id":id,"receipt":published["receipt"]})).unwrap(),
        )
        .unwrap();
    // Model the rename-to-fsync window: a complete manifest alone must not
    // acknowledge completion while its storage job is still in flight.
    write(&f.record(&published["receipt"]), &record);
    let status = m
        .checkpoint_status(
            serde_json::from_value(json!({"id":id,"receipt_id":pending["receipt_id"]})).unwrap(),
        )
        .unwrap();
    assert_eq!(status["status"], "publishing");
    assert!(m
        .restore_async(&target, &published["receipt"])
        .unwrap_err()
        .contains("pending"));
    drop(lock);
    assert_eq!(publication(&mut m, &pending)["status"], "published");
    // Cancellation and a newer generation cannot install an old job's state.
    let lock = CatalogLock::new(&f);
    let ticket = stage(&mut m, &id, 1);
    let before = Instant::now();
    m.stop(&id).unwrap();
    assert!(before.elapsed() < Duration::from_secs(1));
    m.start(&id).unwrap();
    let uncertain = publication(&mut m, &ticket);
    assert_eq!(uncertain["status"], "uncertain");
    assert_eq!(m.status(&id).unwrap()["generation"], 2);
    assert_eq!(m.status(&id).unwrap()["state"], "running");
    drop(lock);
}
fn publish_receipt(m: &mut WorldManager, id: &str, receipt: &Value) -> Value {
    publish(m, id, receipt)
}
#[test]
fn profiles_lock_stores_and_missing_catalog_is_truthful() {
    let f = Fixture::new(false);
    let mut m = f.manager();
    let id = make(&mut m, false);
    m.start(&id).unwrap();
    let ticket = stage(&mut m, &id, 1);
    let staged = publication(&mut m, &ticket);
    assert_eq!(staged["status"], "staged", "{staged}");
    let mut other = WorldManager::new(
        f.root.join("other-registry"),
        f.root.join("other-worlds"),
        f.driver.clone(),
    )
    .unwrap();
    assert!(other
        .configure_storage(&f.root.join("profile.json"))
        .is_err());
    let db = f.storage().join("catalog.sqlite");
    fs::rename(&db, db.with_extension("saved")).unwrap();
    let uncertain = publish(&mut m, &id, &staged["receipt"]);
    assert_eq!(uncertain["status"], "uncertain");
    assert!(uncertain["error"].as_str().unwrap().contains("missing"));
    assert_eq!(m.status(&id).unwrap()["state"], "running");
    assert_eq!(
        m.status(&id).unwrap()["persistence"]["iceberg"]["admission_durability"],
        "json"
    );
    // JSON admission is unaffected by unavailable Iceberg storage.
    let admitted=m.admit_inputs(serde_json::from_value(json!({"id":id,"expected_generation":1,"expected_revision":1,"admission_key":"json-independent","changes":[],"effect":{"key":"external","phase":"reserve"}})).unwrap()).unwrap();
    assert_eq!(admitted["state"], "durable");
    assert_eq!(admitted["receipt"]["format"], "json");
    assert_eq!(admitted["effect_authorized"], true);
}

#[test]
fn storage_startup_flag_and_async_wire_are_reusable() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    let f = Fixture::new(false);
    let mut m = f.manager();
    let id = make(&mut m, false);
    drop(m);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ddlog-worlds"))
        .arg(f.root.join("registry"))
        .arg(f.root.join("worlds"))
        .arg(&f.driver)
        .arg("--storage-profile")
        .arg(f.root.join("profile.json"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut call = |operation: &str, args: Value| -> Value {
        writeln!(input, "{}", json!({"operation":operation,"args":args})).unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ok"], true, "{value}");
        value["result"].clone()
    };
    call("start", json!({"id":id}));
    let deadline = Instant::now() + Duration::from_secs(20);
    while call("status", json!({"id":id}))["state"] == "starting" {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let ticket = call(
        "checkpoint_stage",
        json!({"id":id,"expected_generation":1,"expected_revision":1,"profile":"local"}),
    );
    assert_eq!(ticket["status"], "staging");
    let query = json!({"id":id,"receipt_id":ticket["receipt_id"]});
    let staged = loop {
        let value = call("checkpoint_status", query.clone());
        if value["status"] != "staging" {
            break value;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(staged["status"], "staged", "{staged}");
    assert_eq!(
        call(
            "checkpoint_publish",
            json!({"id":id,"receipt":staged["receipt"]})
        )["status"],
        "publishing"
    );
    let published = loop {
        let value = call("checkpoint_status", query.clone());
        if value["status"] != "publishing" {
            break value;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(published["status"], "published", "{published}");
    assert!(published["receipt"]["storage"]["snapshot_id"].is_string());
    call("stop", json!({"id":id}));
    drop(input);
    assert!(child.wait().unwrap().success());
}
