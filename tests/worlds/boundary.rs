//! Simulated native transport: external publication, not native DDlog evidence.
use super::*;

fn external_definition(manager: &WorldManager) -> WorldDefinition {
    let mut def = definition(manager);
    def.external_publication = Some(ddlog_runtime::worlds::ExternalPublicationPolicy {
        namespace: "archetype".into(),
        outputs: vec!["echo".into()],
        max_rows: 1000,
        max_bytes: 1024 * 1024,
    });
    def
}
#[test]
fn external_policy_rejects_legacy_mutation_before_any_native_command() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = external_definition(&manager);
    let id = manager.create(def).unwrap();
    manager.start(&id).unwrap();
    let before = fs::read(f.root.join("commands")).unwrap();
    for operation in [
        "apply_changes",
        "lemmalog_apply",
        "submit_agent_input",
        "claim_agent_request",
        "complete_agent_request",
    ] {
        assert!(
            manager
                .execute(
                    &id,
                    operation,
                    &json!({"changes":[
                        {"op":"insert","predicate":"source","values":[1,"bypass"]}
                    ]})
                )
                .is_err(),
            "legacy mutation accepted: {operation}"
        );
    }
    assert_eq!(fs::read(f.root.join("commands")).unwrap(), before);
}

use ddlog_runtime::worlds::{BoundaryKey, ExternalReceipt, FrozenManifest};
fn digest(value: &Value) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(serde_json::to_vec(value).unwrap()))
}
fn request(id: &str, generation: u64, revision: u64, key: &str, parent: Option<&str>) -> Value {
    json!({"admission":{"id":id,"expected_generation":generation,"expected_revision":revision,
        "admission_key":key,"changes":[{"op":"insert","predicate":"source","values":[revision,"data"]}]},
        "binding":{"context":{"world":"analytical","run":"run-a","tick":revision},"parent_receipt_sha256":parent}})
}
fn admit(m: &mut WorldManager, value: Value) -> Value {
    m.admit_boundary_async(serde_json::from_value(value).unwrap())
        .unwrap()
}
fn key(value: &Value) -> BoundaryKey {
    serde_json::from_value(value["boundary"]["key"].clone()).unwrap()
}
fn lookup(m: &mut WorldManager, key: &BoundaryKey) -> Value {
    m.admission_status(serde_json::from_value(json!({"id":key.world_id,"generation":key.generation,"admission_key":key.admission_key})).unwrap()).unwrap()
}
fn finished(m: &mut WorldManager, key: &BoundaryKey) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let value = lookup(m, key);
        if value["state"] != "pending" {
            return value;
        }
        assert!(std::time::Instant::now() < deadline, "{value}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
fn receipt(value: &Value) -> ExternalReceipt {
    let body = json!({"cut":"host-cut","native":value["boundary"]["manifest"]["revision"]});
    let frozen_manifest_sha256 = digest(&value["boundary"]["manifest"]);
    let receipt_sha256 =
        digest(&json!({"frozen_manifest_sha256":frozen_manifest_sha256,"receipt":body}));
    ExternalReceipt {
        frozen_manifest_sha256,
        receipt_sha256,
        receipt: body,
    }
}
fn commits(f: &Fixture) -> usize {
    fs::read_to_string(f.root.join("commands"))
        .unwrap()
        .lines()
        .filter(|l| l.starts_with("commit"))
        .count()
}
fn blob(m: &mut WorldManager, key: &BoundaryKey, sha: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut offset = 0;
    loop {
        let page = m
            .read_boundary_blob(
                serde_json::from_value(
                    json!({"key":key,"blob_sha256":sha,"offset":offset,"max_bytes":127}),
                )
                .unwrap(),
            )
            .unwrap();
        bytes.extend(page.bytes);
        match page.next_offset {
            Some(next) => offset = next,
            None => break,
        }
    }
    bytes
}
#[test]
fn exact_cut_parent_ack_and_capture_retry_never_replay_inputs() {
    let f = Fixture::new();
    let mut m = f.manager();
    let def = external_definition(&m);
    let id = m.create(def).unwrap();
    m.start(&id).unwrap();
    let before = commits(&f);
    let original = request(&id, 1, 1, "one", None);
    // Freeze persistence failure after native acknowledgement, with a healthy child.
    let blocked = f.root.join("worlds").join(&id).join("boundaries");
    fs::write(&blocked, "blocked").unwrap();
    let k = key(&admit(&mut m, original.clone()));
    let failed = finished(&mut m, &k);
    assert_eq!(failed["state"], "applied_but_unpublished");
    assert_eq!(commits(&f), before + 1);
    assert_eq!(
        admit(&mut m, original.clone())["state"],
        "applied_but_unpublished"
    );
    let mut conflict = original.clone();
    conflict["binding"]["context"]["tick"] = json!(999);
    assert!(m
        .admit_boundary_async(serde_json::from_value(conflict).unwrap())
        .is_err());
    assert!(m
        .admit_boundary_async(serde_json::from_value(request(&id, 1, 2, "blocked", None)).unwrap())
        .is_err());
    assert!(m.start_async(&id).is_err());
    // A plain internal checkpoint never releases external admission.
    m.checkpoint(&id).unwrap();
    assert!(m.status(&id).unwrap()["external_publication"]["pending"].is_string());
    fs::remove_file(blocked).unwrap();
    m.retry_boundary_freeze_async(k.clone()).unwrap();
    let frozen = finished(&mut m, &k);
    assert_eq!(frozen["state"], "frozen", "{frozen}");
    assert_eq!(commits(&f), before + 1);
    let manifest: FrozenManifest =
        serde_json::from_value(frozen["boundary"]["manifest"].clone()).unwrap();
    let rows: Value =
        serde_json::from_slice(&blob(&mut m, &k, &manifest.outputs[0].blob.sha256)).unwrap();
    assert_eq!(rows, json!([[1, "data"]]));
    let ack = receipt(&frozen);
    m.confirm_boundary_published(k.clone(), ack.clone())
        .unwrap();
    assert!(m
        .admit_boundary_async(serde_json::from_value(request(&id, 1, 2, "stale", None)).unwrap())
        .is_err());
    let next = admit(&mut m, request(&id, 1, 2, "two", Some(&ack.receipt_sha256)));
    // An old exact ack is still idempotent while a later boundary is pending.
    assert_eq!(
        m.confirm_boundary_published(k.clone(), ack.clone())
            .unwrap()["state"],
        "published"
    );
    let mut different = ack.clone();
    different.receipt = json!({"different":true});
    different.receipt_sha256 = digest(
        &json!({"frozen_manifest_sha256":different.frozen_manifest_sha256,"receipt":different.receipt}),
    );
    assert!(m.confirm_boundary_published(k, different).is_err());
    finished(&mut m, &key(&next));
    assert_eq!(commits(&f), before + 2);
}
#[test]
fn failed_initial_activation_can_retry_without_a_nonexistent_checkpoint() {
    let f = Fixture::new();
    let mut m = f.manager();
    let def = external_definition(&m);
    let id = m.create(def).unwrap();
    fs::write(f.root.join("reject_build"), "").unwrap();
    assert!(m.start(&id).is_err());
    fs::remove_file(f.root.join("reject_build")).unwrap();
    assert_eq!(m.start(&id).unwrap()["state"], "running");
}
#[test]
fn freeze_survives_stop_restart_and_lost_ack_without_native_replay() {
    let f = Fixture::new();
    let mut m = f.manager();
    let def = external_definition(&m);
    let id = m.create(def).unwrap();
    m.start(&id).unwrap();
    let original = request(&id, 1, 1, "one", None);
    let k = key(&admit(&mut m, original.clone()));
    let frozen = finished(&mut m, &k);
    let ack = receipt(&frozen);
    let count = commits(&f);
    let world_file = f.root.join("worlds").join(&id).join("world.json");
    let before = fs::read(&world_file).unwrap();
    fs::remove_file(&world_file).unwrap();
    fs::create_dir(&world_file).unwrap();
    assert!(m
        .confirm_boundary_published(k.clone(), ack.clone())
        .is_err());
    assert!(m.status(&id).unwrap()["external_publication"]["pending"].is_string());
    let mut conflict = ack.clone();
    conflict.receipt = json!({"changed":true});
    conflict.receipt_sha256 = digest(
        &json!({"frozen_manifest_sha256":conflict.frozen_manifest_sha256,"receipt":conflict.receipt}),
    );
    assert!(m.confirm_boundary_published(k.clone(), conflict).is_err());
    fs::remove_dir(&world_file).unwrap();
    fs::write(&world_file, before).unwrap();
    m.stop(&id).unwrap();
    drop(m);
    let mut m = f.manager();
    assert_eq!(lookup(&mut m, &k)["state"], "frozen");
    assert!(m.start_async(&id).is_err());
    let manifest: FrozenManifest =
        serde_json::from_value(frozen["boundary"]["manifest"].clone()).unwrap();
    assert!(!blob(&mut m, &k, &manifest.checkpoint.sha256).is_empty());
    m.confirm_boundary_published(k.clone(), ack.clone())
        .unwrap();
    drop(m);
    let mut m = f.manager();
    assert_eq!(
        m.confirm_boundary_published(k, ack).unwrap()["state"],
        "published"
    );
    assert_eq!(commits(&f), count);
}
#[test]
fn uncertain_apply_remains_blocked_across_stop_and_restart() {
    let f = Fixture::new();
    let mut m = f.manager();
    let def = external_definition(&m);
    let id = m.create(def).unwrap();
    m.start(&id).unwrap();
    let before = commits(&f);
    fs::write(f.root.join("die_on_commit"), "").unwrap();
    let req = request(&id, 1, 1, "uncertain", None);
    let k = key(&admit(&mut m, req.clone()));
    assert_eq!(finished(&mut m, &k)["state"], "uncertain");
    assert!(m.retry_boundary_freeze_async(k.clone()).is_err());
    fs::remove_file(f.root.join("die_on_commit")).unwrap();
    m.stop(&id).unwrap();
    drop(m);
    let mut m = f.manager();
    assert_eq!(admit(&mut m, req)["state"], "uncertain");
    assert!(m.start_async(&id).is_err());
    assert!(m
        .admit_boundary_async(serde_json::from_value(request(&id, 1, 2, "next", None)).unwrap())
        .is_err());
    assert_eq!(commits(&f), before + 1);
}
#[test]
fn bound_restore_preserves_origin_and_new_activation_and_retraction() {
    let f = Fixture::new();
    let mut m = f.manager();
    let def = external_definition(&m);
    let id = m.create(def).unwrap();
    m.start(&id).unwrap();
    let k = key(&admit(&mut m, request(&id, 1, 1, "one", None)));
    let frozen = finished(&mut m, &k);
    let manifest: FrozenManifest =
        serde_json::from_value(frozen["boundary"]["manifest"].clone()).unwrap();
    let ack = receipt(&frozen);
    let bytes = blob(&mut m, &k, &manifest.checkpoint.sha256);
    m.confirm_boundary_published(k, ack.clone()).unwrap();
    m.stop(&id).unwrap();
    assert!(m.restore_async(&id, &manifest.checkpoint_receipt).is_err());
    let mut restore = json!({"target_world_id":id,"expected_generation":1,"manifest":manifest,"checkpoint_bytes":bytes,"published":ack});
    restore["checkpoint_bytes"][0] = json!(0);
    assert!(m
        .restore_boundary_async(serde_json::from_value(restore.clone()).unwrap())
        .is_err());
    restore["checkpoint_bytes"] = json!(bytes);
    m.restore_boundary_async(serde_json::from_value(restore).unwrap())
        .unwrap();
    let status = wait_until(&mut m, &id, |s| s["state"] != "starting");
    assert_eq!(status["state"], "running");
    assert_eq!(status["generation"], 2);
    assert_eq!(
        status["persistence"]["restored_from"]["origin"]["generation"],
        1
    );
    assert_eq!(status["instance"]["revision"], 2);
    assert!(status["instance"]["build"]["native_sha256"].is_string());
    let mut retract = request(&id, 2, 2, "retract", Some(&ack.receipt_sha256));
    retract["admission"]["changes"] =
        json!([{"op":"delete","predicate":"source","values":[1,"data"]}]);
    let next = key(&admit(&mut m, retract));
    let empty = finished(&mut m, &next);
    assert_eq!(empty["state"], "frozen");
    assert_eq!(empty["boundary"]["manifest"]["outputs"][0]["rows"], 0);
    let sha = empty["boundary"]["manifest"]["outputs"][0]["blob"]["sha256"]
        .as_str()
        .unwrap();
    assert_eq!(blob(&mut m, &next, sha), b"[]");
}
#[test]
fn held_native_apply_keeps_status_stop_and_other_world_responsive() {
    let f = Fixture::new();
    let mut m = f.manager();
    let def = external_definition(&m);
    let a = m.create(def.clone()).unwrap();
    let b = m.create(def).unwrap();
    let status = m.start(&a).unwrap();
    m.start(&b).unwrap();
    let pid = status["resources"]["pid"].as_u64().unwrap();
    fs::write(f.root.join(format!("hold_commit_{pid}")), "").unwrap();
    let akey = key(&admit(&mut m, request(&a, 1, 1, "a", None)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !f.root.join(format!("commit_held_{pid}")).exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let start = std::time::Instant::now();
    assert_eq!(m.status(&a).unwrap()["external_publication"]["busy"], true);
    let bkey = key(&admit(&mut m, request(&b, 1, 1, "b", None)));
    assert_eq!(finished(&mut m, &bkey)["state"], "frozen");
    m.stop(&a).unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(3));
    assert_eq!(finished(&mut m, &akey)["state"], "uncertain");
    assert_eq!(m.status(&b).unwrap()["state"], "running");
}

#[test]
fn complete_preflight_and_durable_intent_precede_native_mutation() {
    let f = Fixture::new();
    let mut m = f.manager();
    let def = external_definition(&m);
    let id = m.create(def).unwrap();
    m.start(&id).unwrap();
    let before = commits(&f);
    let mut bad = request(&id, 1, 1, "bad", None);
    bad["admission"]["changes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"op":"insert","predicate":"echo","values":[2,"bad"]}));
    assert!(m
        .admit_boundary_async(serde_json::from_value(bad).unwrap())
        .is_err());
    let ordinary = request(&id, 1, 1, "ordinary", None)["admission"].clone();
    assert_eq!(
        m.admit_inputs(serde_json::from_value(ordinary).unwrap())
            .unwrap()["state"],
        "not_applied"
    );
    assert!(m.worker_start(serde_json::from_value(json!({"id":id,"expected_generation":1,"profile":"none","key":"worker","payload":{}})).unwrap()).is_err());
    assert_eq!(commits(&f), before);
    let world_file = f.root.join("worlds").join(&id).join("world.json");
    let original = fs::read(&world_file).unwrap();
    fs::remove_file(&world_file).unwrap();
    fs::create_dir(&world_file).unwrap();
    assert!(m
        .admit_boundary_async(
            serde_json::from_value(request(&id, 1, 1, "blocked-intent", None)).unwrap()
        )
        .is_err());
    assert_eq!(commits(&f), before);
    assert!(m.status(&id).unwrap()["external_publication"]["pending"].is_string());
    fs::remove_dir(&world_file).unwrap();
    fs::write(world_file, original).unwrap();
    assert!(m
        .admit_boundary_async(
            serde_json::from_value(request(&id, 1, 1, "replacement", None)).unwrap()
        )
        .is_err());
    assert_eq!(commits(&f), before);
}
#[test]
fn opaque_receipt_reuse_cannot_alias_two_analytical_parents() {
    let f = Fixture::new();
    let mut m = f.manager();
    let def = external_definition(&m);
    let id = m.create(def).unwrap();
    m.start(&id).unwrap();
    let mut parent = None;
    let mut first = None;
    for revision in [1, 2] {
        let k = key(&admit(
            &mut m,
            request(
                &id,
                1,
                revision,
                &format!("cut-{revision}"),
                parent.as_deref(),
            ),
        ));
        let frozen = finished(&mut m, &k);
        let mut ack = receipt(&frozen);
        ack.receipt = json!({});
        ack.receipt_sha256 = digest(
            &json!({"frozen_manifest_sha256":ack.frozen_manifest_sha256,"receipt":ack.receipt}),
        );
        m.confirm_boundary_published(k, ack.clone()).unwrap();
        if first.is_none() {
            first = Some(ack.receipt_sha256.clone())
        }
        parent = Some(ack.receipt_sha256);
    }
    assert_ne!(first, parent);
    assert!(m
        .admit_boundary_async(
            serde_json::from_value(request(&id, 1, 3, "stale", first.as_deref())).unwrap()
        )
        .is_err());
}
#[test]
fn invalid_pinned_import_does_not_poison_a_fresh_targets_head() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new();
    let mut m = f.manager();
    let def = external_definition(&m);
    let id = m.create(def.clone()).unwrap();
    m.start(&id).unwrap();
    let k = key(&admit(&mut m, request(&id, 1, 1, "one", None)));
    let frozen = finished(&mut m, &k);
    let manifest: FrozenManifest =
        serde_json::from_value(frozen["boundary"]["manifest"].clone()).unwrap();
    let ack = receipt(&frozen);
    let bytes = blob(&mut m, &k, &manifest.checkpoint.sha256);
    m.confirm_boundary_published(k, ack.clone()).unwrap();
    let target = m.create(def).unwrap();
    let mut bad = manifest.clone();
    let mut checkpoint: Value = serde_json::from_slice(&bytes).unwrap();
    checkpoint["state"]["source"] = json!(format!(
        "{}\n// corrupt pinned source\n",
        checkpoint["state"]["source"].as_str().unwrap()
    ));
    checkpoint["sha256"] = json!(digest(&checkpoint["state"]));
    bad.checkpoint_receipt["checkpoint_sha256"] = checkpoint["sha256"].clone();
    let forged = serde_json::to_vec(&checkpoint).unwrap();
    bad.checkpoint.sha256 = format!("{:x}", Sha256::digest(&forged));
    bad.checkpoint.bytes = forged.len() as u64;
    let mut bad_ack = ack.clone();
    bad_ack.frozen_manifest_sha256 = digest(&json!(bad));
    bad_ack.receipt_sha256 = digest(
        &json!({"frozen_manifest_sha256":bad_ack.frozen_manifest_sha256,"receipt":bad_ack.receipt}),
    );
    assert!(m
        .restore_boundary_async(
            serde_json::from_value(json!({"target_world_id":target,"expected_generation":0,
        "manifest":bad,"checkpoint_bytes":forged,"published":bad_ack}))
            .unwrap()
        )
        .is_err());
    let status = m.status(&target).unwrap();
    assert_eq!(status["generation"], 0);
    assert!(status["external_publication"]["head"].is_null());
    m.restore_boundary_async(
        serde_json::from_value(json!({"target_world_id":target,"expected_generation":0,
        "manifest":manifest,"checkpoint_bytes":bytes,"published":ack}))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        wait_until(&mut m, &target, |s| s["state"] != "starting")["state"],
        "running"
    );
}
#[test]
fn durable_frozen_manifest_recovers_a_stale_intent_record_without_replay() {
    let f = Fixture::new();
    let mut m = f.manager();
    let def = external_definition(&m);
    let id = m.create(def).unwrap();
    let pid = m.start(&id).unwrap()["resources"]["pid"].as_u64().unwrap();
    let hold = f.root.join(format!("hold_commit_{pid}"));
    fs::write(&hold, "").unwrap();
    let k = key(&admit(&mut m, request(&id, 1, 1, "one", None)));
    let world_file = f.root.join("worlds").join(&id).join("world.json");
    let pending = fs::read(&world_file).unwrap();
    fs::remove_file(hold).unwrap();
    let frozen = finished(&mut m, &k);
    assert_eq!(frozen["state"], "frozen");
    let before = commits(&f);
    drop(m);
    fs::write(world_file, pending).unwrap();
    let mut m = f.manager();
    let recovered = lookup(&mut m, &k);
    assert_eq!(recovered["state"], "frozen");
    assert_eq!(
        recovered["boundary"]["manifest"],
        frozen["boundary"]["manifest"]
    );
    assert!(m.start_async(&id).is_err());
    m.confirm_boundary_published(k, receipt(&frozen)).unwrap();
    assert_eq!(commits(&f), before);
}
#[test]
fn completion_persistence_failure_retains_inspectable_frozen_evidence() {
    let f = Fixture::new();
    let mut m = f.manager();
    let def = external_definition(&m);
    let id = m.create(def).unwrap();
    let pid = m.start(&id).unwrap()["resources"]["pid"].as_u64().unwrap();
    let hold = f.root.join(format!("hold_commit_{pid}"));
    fs::write(&hold, "").unwrap();
    let original = request(&id, 1, 1, "one", None);
    let k = key(&admit(&mut m, original.clone()));
    let path = f.root.join("worlds").join(&id).join("world.json");
    let intent = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    fs::remove_file(hold).unwrap();
    let frozen = finished(&mut m, &k);
    assert_eq!(frozen["state"], "frozen");
    assert!(frozen["error"].as_str().unwrap().contains("persistence"));
    assert_eq!(admit(&mut m, original)["state"], "frozen");
    assert!(m.status(&id).is_ok());
    assert!(m
        .admit_boundary_async(serde_json::from_value(request(&id, 1, 2, "blocked", None)).unwrap())
        .is_err());
    fs::remove_dir(&path).unwrap();
    fs::write(path, intent).unwrap();
    m.confirm_boundary_published(k, receipt(&frozen)).unwrap();
}

#[test]
fn bounded_json_depth_precedes_admission_and_publication_persistence() {
    let f = Fixture::new();
    let mut m = f.manager();
    let def = external_definition(&m);
    let id = m.create(def).unwrap();
    m.start(&id).unwrap();
    let before = commits(&f);
    let mut context = Value::Null;
    for _ in 0..125 {
        context = json!({"nested":context});
    }
    let mut deep: ddlog_runtime::worlds::BoundaryAdmission =
        serde_json::from_value(request(&id, 1, 1, "deep", None)).unwrap();
    deep.binding.context = context.clone();
    assert!(
        m.admit_boundary_async(deep).is_err(),
        "deep binding admitted"
    );
    assert_eq!(commits(&f), before);
    let k = key(&admit(&mut m, request(&id, 1, 1, "safe", None)));
    let frozen = finished(&mut m, &k);
    let mut ack = receipt(&frozen);
    ack.receipt = context;
    ack.receipt_sha256 =
        digest(&json!({"frozen_manifest_sha256":ack.frozen_manifest_sha256,"receipt":ack.receipt}));
    assert!(
        m.confirm_boundary_published(k.clone(), ack).is_err(),
        "deep receipt published"
    );
    drop(m);
    let mut m = f.manager();
    assert_eq!(lookup(&mut m, &k)["state"], "frozen");
}
#[test]
fn full_output_byte_limit_counts_combined_rows_across_page_boundaries() {
    let f = Fixture::new();
    let mut m = f.manager();
    let mut def = external_definition(&m);
    let rows: Vec<Value> = (1..=1001).map(|i| json!([i, "data"])).collect();
    let policy = def.external_publication.as_mut().unwrap();
    policy.max_rows = 2000;
    policy.max_bytes = serde_json::to_vec(&rows).unwrap().len();
    let id = m.create(def).unwrap();
    m.start(&id).unwrap();
    let mut first = request(&id, 1, 1, "page-one", None);
    first["admission"]["changes"] = json!((1..=1000)
        .map(|i| json!({"op":"insert","predicate":"source","values":[i,"data"]}))
        .collect::<Vec<_>>());
    let k = key(&admit(&mut m, first));
    let frozen = finished(&mut m, &k);
    assert_eq!(frozen["state"], "frozen", "{frozen}");
    let ack = receipt(&frozen);
    m.confirm_boundary_published(k, ack.clone()).unwrap();
    let mut second = request(&id, 1, 2, "page-two", Some(&ack.receipt_sha256));
    second["admission"]["changes"][0]["values"] = json!([1001, "data"]);
    let k = key(&admit(&mut m, second));
    let frozen = finished(&mut m, &k);
    assert_eq!(frozen["state"], "frozen", "{frozen}");
    assert_eq!(frozen["boundary"]["manifest"]["outputs"][0]["rows"], 1001);
    assert_eq!(
        frozen["boundary"]["manifest"]["outputs"][0]["blob"]["bytes"],
        serde_json::to_vec(&rows).unwrap().len()
    );
}
