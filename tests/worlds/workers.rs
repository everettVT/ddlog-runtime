//! Process ownership, profile validation and cancellation without any provider.
use super::*;
fn profiles(f: &Fixture, pin: &ProcessorReference, timeout: u64) -> PathBuf {
    let path = f.root.join("profiles.json");
    fs::write(&path,serde_json::to_vec(&json!({"schema_version":1,"profiles":{"fixture":{
        "argv":["/usr/bin/env","python3",PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/managed_worker.py")],
        "env":{"FIXTURE_ROOT":f.root},"config_ref":f.root.join("configuration.json"),
        "allowed_processors":[pin],"max_concurrent_workers":1,"timeout_ms":timeout
    }}})).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}
fn start(manager: &mut WorldManager, id: &str, key: &str, mode: &str) -> Result<Value, String> {
    manager.worker_start(
        serde_json::from_value(
            json!({"id":id,"expected_generation":1,"profile":"fixture","key":key,
        "payload":{"mode":mode,"private":"not in status"}}),
        )
        .unwrap(),
    )
}
fn wait(manager: &mut WorldManager, id: &str, worker: &Value) -> Value {
    let status = wait_until(manager, id, |s| {
        s["workers"].as_array().unwrap().iter().any(|w| {
            w["worker_id"] == worker["worker_id"]
                && !matches!(
                    w["state"].as_str().unwrap(),
                    "running" | "starting" | "stopping"
                )
        })
    });
    status["workers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["worker_id"] == worker["worker_id"])
        .unwrap()
        .clone()
}
fn wait_file(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !path.exists() {
        assert!(std::time::Instant::now() < deadline, "{}", path.display());
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
#[test]
fn startup_profiles_exact_pins_keys_and_safe_retained_outcomes() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let path = profiles(&f, &def.processor, 5000);
    manager
        .configure_workers(&path, f.root.join("owner.json"))
        .unwrap();
    let id = manager.create(def).unwrap();
    manager.start(&id).unwrap();
    assert!(manager
        .configure_workers(&path, f.root.join("owner.json"))
        .is_err());
    let worker = start(&mut manager, &id, "run", "complete").unwrap();
    let pid = worker["pid"].as_u64().unwrap() as i32;
    let complete = wait(&mut manager, &id, &worker);
    assert_eq!(complete["state"], "completed");
    assert_eq!(complete["exit_code"], 0);
    assert_eq!(complete["outcome"]["code"], "fixture_ok");
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    let text = complete.to_string();
    assert!(!text.contains("not in status"));
    assert!(!text.contains("configuration.json"));
    assert!(!text.contains("argv"));
    let envelope: Value =
        serde_json::from_slice(&fs::read(f.root.join("run.launch.json")).unwrap()).unwrap();
    assert_eq!(envelope["worker_id"], worker["worker_id"]);
    assert_eq!(
        envelope["owner_descriptor"],
        json!(f.root.join("owner.json"))
    );
    assert_eq!(
        start(&mut manager, &id, "run", "complete").unwrap()["replayed"],
        true
    );
    assert!(start(&mut manager, &id, "run", "hold").is_err());
    let wrong = definition(&manager);
    let wrong = manager.create(wrong).unwrap();
    manager.start(&wrong).unwrap();
    assert!(start(&mut manager, &wrong, "wrong-pin", "complete")
        .unwrap_err()
        .contains("pin"));
    drop(manager);
    let mut manager = f.manager();
    let status = manager
        .worker_status(
            serde_json::from_value(json!({"id":id,"worker_id":worker["worker_id"]})).unwrap(),
        )
        .unwrap();
    assert_eq!(status["workers"][0]["state"], "completed");
    assert_eq!(status["workers"][0]["pid"], pid);
}
#[test]
fn cancellation_capacity_and_world_shutdown_reap_worker_descendants() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let path = profiles(&f, &def.processor, 30000);
    manager
        .configure_workers(&path, f.root.join("owner.json"))
        .unwrap();
    let id = manager.create(def.clone()).unwrap();
    manager.start(&id).unwrap();
    let other = manager.create(def).unwrap();
    manager.start(&other).unwrap();
    let worker = start(&mut manager, &id, "run", "descendant").unwrap();
    wait_file(&f.root.join("run.child"));
    let child = fs::read_to_string(f.root.join("run.child"))
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert!(start(&mut manager, &other, "other", "hold")
        .unwrap_err()
        .contains("concurrency"));
    let status = manager.status(&id).unwrap();
    assert!(status["managed_processes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["role"] == "worker" && p["sample"]["pid"] == worker["pid"]));
    manager
        .worker_stop(
            serde_json::from_value(
                json!({"id":id,"expected_generation":1,"worker_id":worker["worker_id"]}),
            )
            .unwrap(),
        )
        .unwrap();
    let late = manager
        .admit_inputs(
            serde_json::from_value(
                json!({"id":id,"expected_generation":1,"expected_revision":1,
        "admission_key":"late","worker_id":worker["worker_id"],"changes":[]}),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(late["state"], "not_applied");
    assert_eq!(wait(&mut manager, &id, &worker)["state"], "stopped");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while unsafe { libc::kill(child, 0) } == 0 {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        manager.status(&id).unwrap()["state"],
        "running",
        "worker_stop must not kill native"
    );
    let worker = start(&mut manager, &id, "stop-world", "hold").unwrap();
    manager.stop(&id).unwrap();
    assert_eq!(wait(&mut manager, &id, &worker)["state"], "stopped");
    let worker = start(&mut manager, &other, "owner-exit", "hold").unwrap();
    drop(manager);
    assert_eq!(
        unsafe { libc::kill(worker["pid"].as_u64().unwrap() as i32, 0) },
        -1
    );
    let mut manager = f.manager();
    assert_eq!(
        manager.status(&other).unwrap()["workers"][0]["state"],
        "stopped"
    );
}
#[test]
fn malformed_oversized_nonzero_and_timeout_outcomes_fail_boundedly() {
    for (mode, timeout) in [
        ("invalid", 5000),
        ("oversized", 5000),
        ("exit", 5000),
        ("hold", 200),
    ] {
        let f = Fixture::new();
        let mut manager = f.manager();
        let def = definition(&manager);
        let path = profiles(&f, &def.processor, timeout);
        manager
            .configure_workers(&path, f.root.join("owner.json"))
            .unwrap();
        let id = manager.create(def).unwrap();
        manager.start(&id).unwrap();
        let worker = start(&mut manager, &id, "run", mode).unwrap();
        let failed = wait(&mut manager, &id, &worker);
        assert_eq!(failed["state"], "failed");
        assert!(!failed.to_string().contains("never expose"));
        assert_eq!(
            start(&mut manager, &id, "run", mode).unwrap()["replayed"],
            true
        );
        assert_eq!(
            unsafe { libc::kill(worker["pid"].as_u64().unwrap() as i32, 0) },
            -1
        );
    }
}
#[test]
fn nonzero_exit_retains_only_valid_failure_outcomes_across_restart() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let path = profiles(&f, &def.processor, 5000);
    manager
        .configure_workers(&path, f.root.join("owner.json"))
        .unwrap();
    let id = manager.create(def).unwrap();
    manager.start(&id).unwrap();
    let mut expected = Vec::new();
    for mode in [
        "failed_exit",
        "completed_exit",
        "invalid_exit",
        "unknown_exit",
    ] {
        let worker = start(&mut manager, &id, mode, mode).unwrap();
        let failed = wait(&mut manager, &id, &worker);
        assert_eq!(failed["state"], "failed");
        assert_eq!(failed["exit_code"], 9);
        if mode == "failed_exit" {
            assert_eq!(
                failed["outcome"],
                json!({"schema_version":1,"status":"failed","code":"configuration_invalid"})
            );
        } else {
            assert!(failed["outcome"].is_null());
        }
        assert!(!failed.to_string().contains("never expose"));
        assert_eq!(
            start(&mut manager, &id, mode, mode).unwrap()["replayed"],
            true
        );
        expected.push(failed["outcome"].clone());
    }
    drop(manager);
    let mut manager = f.manager();
    let status = manager.status(&id).unwrap();
    for (worker, outcome) in status["workers"].as_array().unwrap().iter().zip(expected) {
        assert_eq!(worker["state"], "failed");
        assert_eq!(worker["outcome"], outcome);
        assert_eq!(worker["exit_code"], 9);
    }
}

#[test]
fn unsafe_or_mutable_profiles_and_unknown_launch_fields_are_rejected() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let path = profiles(&f, &def.processor, 5000);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(manager
        .configure_workers(&path, f.root.join("owner.json"))
        .is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let link = f.root.join("symlink.json");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(manager
        .configure_workers(&link, f.root.join("owner.json"))
        .is_err());
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["profiles"]["fixture"]["argv"][0] = json!("relative");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(manager
        .configure_workers(&path, f.root.join("owner.json"))
        .is_err());
    assert!(serde_json::from_value::<ddlog_runtime::worlds::WorkerStart>(json!({"id":"world","expected_generation":1,"profile":"fixture","key":"run","payload":{},"argv":["/bin/sh"]})).is_err());
}
