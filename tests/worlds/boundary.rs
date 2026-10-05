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

fn fresh_creation(m: &WorldManager) -> ddlog_runtime::worlds::CreationRequest {
    ddlog_runtime::worlds::CreationRequest {
        request_key: "logical-one".into(),
        destination: ddlog_runtime::worlds::LogicalDestination {
            resource: "world-a".into(),
            world: "logical".into(),
            run: "one".into(),
        },
        definition: external_definition(m),
        binding: json!({"components":["echo"]}),
        fork: None,
    }
}
#[test]
fn logical_fresh_creation_recovers_each_fence_and_requires_context_before_activation() {
    use ddlog_runtime::worlds::ForkFault;
    for fault in [
        ForkFault::AfterReservation,
        ForkFault::AfterChildRecord,
        ForkFault::ChildDirectorySync,
    ] {
        let f = Fixture::new();
        let mut m = f.manager();
        let request = fresh_creation(&m);
        assert!(m.lookup_creation(&request.destination).unwrap().is_none());
        assert!(!f.root.join("worlds/forks").exists());
        assert!(m
            .reserve_creation_with_fault(request.clone(), fault)
            .is_err());
        assert!(!f.root.join("commands").exists());
        drop(m);
        let mut m = f.manager();
        let resolved = m.resolve_creation(&request.destination).unwrap();
        assert!(!resolved.context_confirmed);
        assert!(resolved.context_id.is_none());
        let reservation = resolved.reservation;
        assert_eq!(m.reserve_creation(request.clone()).unwrap(), reservation);
        let id = &reservation.world_id;
        assert_eq!(m.status(id).unwrap()["generation"], 0);
        assert!(m.start_async(id).is_err());
        for variant in 0..5 {
            let mut changed = request.clone();
            match variant {
                0 => changed.request_key = "other".into(),
                1 => changed.binding = json!({"components":["different"]}),
                2 => changed.destination.resource = "alias".into(),
                3 => changed.destination.world = "other".into(),
                _ => changed.definition.label = "changed".into(),
            }
            assert!(m.reserve_creation(changed).is_err());
        }
        assert_eq!(inventory(&mut m)["worlds"].as_array().unwrap().len(), 1);
        let ready = m
            .confirm_creation_context(reservation.clone(), "a".repeat(64))
            .unwrap();
        assert!(ready.context_confirmed);
        assert!(m
            .confirm_creation_context(reservation.clone(), "b".repeat(64))
            .is_err());
        assert_eq!(m.status(id).unwrap()["generation"], 0);
        assert!(!f.root.join("commands").exists());
        let persisted: Value = serde_json::from_slice(
            &fs::read(f.root.join("worlds").join(id).join("world.json")).unwrap(),
        )
        .unwrap();
        assert!(persisted["admission"]["external_head"].is_null());
        drop(m);
        let mut m = f.manager();
        assert!(
            m.resolve_creation(&request.destination)
                .unwrap()
                .context_confirmed
        );
        m.start(id).unwrap();
        let k = key(&admit(&mut m, self::request(id, 1, 1, "first", None)));
        assert_eq!(finished(&mut m, &k)["state"], "frozen");
    }
}

#[test]
fn failed_creation_context_ack_retains_candidate_and_blocks_start() {
    let f = Fixture::new();
    let mut m = f.manager();
    let request = fresh_creation(&m);
    let reservation = m.reserve_creation(request.clone()).unwrap();
    let block = f
        .root
        .join("worlds")
        .join(&reservation.world_id)
        .join("world.json.tmp");
    fs::create_dir(&block).unwrap();
    assert!(m
        .confirm_creation_context(reservation.clone(), "a".repeat(64))
        .is_err());
    assert!(m
        .confirm_creation_context(reservation.clone(), "b".repeat(64))
        .is_err());
    assert!(m.start_async(&reservation.world_id).is_err());
    let pending = m.resolve_creation(&request.destination).unwrap();
    assert_eq!(pending.context_id, Some("a".repeat(64)));
    assert!(!pending.context_confirmed);
    drop(m);
    fs::remove_dir(block).unwrap();
    let mut m = f.manager();
    let pending = m.resolve_creation(&request.destination).unwrap();
    assert_eq!(pending.context_id, Some("a".repeat(64)));
    assert!(!pending.context_confirmed);
    assert!(m.start_async(&reservation.world_id).is_err());
    assert!(m
        .confirm_creation_context(reservation.clone(), "b".repeat(64))
        .is_err());
    assert!(
        m.confirm_creation_context(reservation, "a".repeat(64))
            .unwrap()
            .context_confirmed
    );
}

#[test]
fn live_owner_rejects_missing_reservations_before_allocating_another_world() {
    for legacy in [false, true] {
        for whole_catalog in [false, true] {
            let f = Fixture::new();
            let mut m = f.manager();
            let mut fresh = fresh_creation(&m);
            let mut fork = fork_request(&mut m, &f);
            fork.destination = json!({"world":fresh.destination.world,"run":fresh.destination.run});
            if legacy {
                m.reserve_fork(fork.clone()).unwrap();
            } else {
                m.reserve_creation(fresh.clone()).unwrap();
            }
            let count = inventory(&mut m)["worlds"].as_array().unwrap().len();
            let catalog = f.root.join("worlds/forks");
            if whole_catalog {
                fs::rename(&catalog, f.root.join("retained-forks")).unwrap();
            } else {
                let path = fs::read_dir(&catalog)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| path.extension().and_then(|x| x.to_str()) == Some("json"))
                    .unwrap();
                fs::remove_file(path).unwrap();
            }
            fresh.request_key = "replacement-fresh".into();
            fork.request_key = "replacement-fork".into();
            assert!(m.lookup_creation(&fresh.destination).is_err());
            assert!(m.reserve_creation(fresh).is_err());
            assert!(m.reserve_fork(fork).is_err());
            assert_eq!(inventory(&mut m)["worlds"].as_array().unwrap().len(), count);
            if whole_catalog {
                assert!(!catalog.exists());
            } else {
                assert!(fs::read_dir(catalog).unwrap().all(|entry| {
                    entry.unwrap().path().extension().and_then(|x| x.to_str()) != Some("json")
                }));
            }
        }
    }
}

#[test]
fn ready_creation_requires_retained_exact_context_candidate() {
    for remove in [false, true] {
        let f = Fixture::new();
        let mut m = f.manager();
        let request = fresh_creation(&m);
        let reservation = m.reserve_creation(request.clone()).unwrap();
        m.confirm_creation_context(reservation, "a".repeat(64))
            .unwrap();
        let path = fs::read_dir(f.root.join("worlds/forks"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().and_then(|x| x.to_str()) == Some("context"))
            .unwrap();
        if remove {
            fs::remove_file(path).unwrap();
        } else {
            let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            value["context_id"] = json!("b".repeat(64));
            fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        assert!(m.resolve_creation(&request.destination).is_err());
        drop(m);
        assert!(WorldManager::new(
            f.root.join("registry"),
            f.root.join("worlds"),
            f.root.join("build.py")
        )
        .is_err());
    }
}

#[test]
fn materialized_logical_world_cannot_be_recreated_after_control_loss() {
    let f = Fixture::new();
    let mut m = f.manager();
    let request = fresh_creation(&m);
    let reservation = m.reserve_creation(request).unwrap();
    m.confirm_creation_context(reservation.clone(), "a".repeat(64))
        .unwrap();
    m.start(&reservation.world_id).unwrap();
    m.stop(&reservation.world_id).unwrap();
    drop(m);
    fs::rename(
        f.root.join("worlds").join(&reservation.world_id),
        f.root.join("retained-world"),
    )
    .unwrap();
    assert!(WorldManager::new(
        f.root.join("registry"),
        f.root.join("worlds"),
        f.root.join("build.py")
    )
    .is_err());
}

#[test]
fn logical_fork_shares_occupancy_and_separates_context_from_lineage_readiness() {
    use ddlog_runtime::worlds::{CreationRequest, LogicalDestination};
    let f = Fixture::new();
    let mut m = f.manager();
    let source = fork_request(&mut m, &f);
    let request = CreationRequest {
        request_key: source.request_key.clone(),
        destination: LogicalDestination {
            resource: "child".into(),
            world: "child".into(),
            run: "one".into(),
        },
        definition: source.definition.clone(),
        binding: json!({"components":["echo"]}),
        fork: Some(Box::new(source.clone())),
    };
    let reservation = m.reserve_creation(request.clone()).unwrap();
    let resolved = m.resolve_creation(&request.destination).unwrap();
    let fork = resolved.fork.unwrap();
    assert!(m
        .restore_fork_async(fork.clone(), 0, source.checkpoint_bytes.clone())
        .is_err());
    let mut fresh = request.clone();
    fresh.request_key = "fresh-collision".into();
    fresh.fork = None;
    assert!(m.reserve_creation(fresh).is_err());
    let mut legacy = source.clone();
    legacy.request_key = "legacy-collision".into();
    assert!(m.reserve_fork(legacy).is_err());
    let persisted: Value = serde_json::from_slice(
        &fs::read(
            f.root
                .join("worlds")
                .join(&reservation.world_id)
                .join("world.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(persisted["admission"]["external_head"].is_null());
    m.confirm_creation_context(reservation.clone(), "a".repeat(64))
        .unwrap();
    assert!(m.start_async(&reservation.world_id).is_err());
    m.restore_fork_async(fork.clone(), 0, source.checkpoint_bytes)
        .unwrap();
    let status = await_fork(&mut m, &reservation.world_id);
    let input = self::request(
        &reservation.world_id,
        1,
        status["revision"].as_u64().unwrap(),
        "child-input",
        Some(&source.published.receipt_sha256),
    );
    assert!(m
        .admit_boundary_async(serde_json::from_value(input.clone()).unwrap())
        .is_err());
    m.confirm_fork_lineage(fork.clone(), fork.lineage_sha256().unwrap())
        .unwrap();
    let k = key(&admit(&mut m, input));
    assert_eq!(finished(&mut m, &k)["state"], "frozen");
    drop(m);
    let mut m = f.manager();
    let recovered = m.resolve_creation(&request.destination).unwrap();
    assert_eq!(recovered.reservation, reservation);
    assert!(recovered.context_confirmed);
}
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

fn fork_request(m: &mut WorldManager, f: &Fixture) -> ddlog_runtime::worlds::ForkRequest {
    let definition = external_definition(m);
    let parent = m.create(definition.clone()).unwrap();
    m.start(&parent).unwrap();
    let k = key(&admit(m, request(&parent, 1, 1, "fork-source", None)));
    let frozen = finished(m, &k);
    let manifest: FrozenManifest =
        serde_json::from_value(frozen["boundary"]["manifest"].clone()).unwrap();
    let checkpoint_bytes = blob(m, &k, &manifest.checkpoint.sha256);
    let published = receipt(&frozen);
    m.confirm_boundary_published(k, published.clone()).unwrap();
    assert!(commits(f) > 0);
    ddlog_runtime::worlds::ForkRequest {
        request_key: "fork-one".into(),
        destination: json!({"world":"child","run":"one"}),
        source_context: json!({"world":"parent","run":"one"}),
        definition,
        manifest,
        published,
        checkpoint_bytes,
    }
}
fn await_fork(m: &mut WorldManager, child: &str) -> Value {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let status = m.status(child).unwrap();
        if status["state"] != "starting" {
            assert_eq!(status["state"], "running", "{status}");
            return status;
        }
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
#[test]
fn fork_reservation_survives_missing_child_and_rejects_conflicting_retries() {
    use ddlog_runtime::worlds::ForkFault;
    let f = Fixture::new();
    let mut m = f.manager();
    let original = fork_request(&mut m, &f);
    let before = commits(&f);
    let mut corrupt = original.clone();
    corrupt.checkpoint_bytes.push(b' ');
    assert!(m.reserve_fork(corrupt).is_err());
    assert!(!f.root.join("worlds/forks").exists());
    assert!(m
        .reserve_fork_with_fault(original.clone(), ForkFault::AfterReservation)
        .is_err());
    let path = fs::read_dir(f.root.join("worlds/forks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let persisted: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let child = persisted["reservation"]["child_world_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!f
        .root
        .join("worlds")
        .join(&child)
        .join("world.json")
        .exists());
    drop(m);
    let mut m = f.manager();
    let reservation = m.reserve_fork(original.clone()).unwrap();
    assert_eq!(reservation.child_world_id, child);
    assert_eq!(m.status(&child).unwrap()["generation"], 0);
    assert_eq!(commits(&f), before);
    for kind in 0..4 {
        let mut changed = original.clone();
        match kind {
            0 => changed.destination = json!({"world":"other","run":"one"}),
            1 => changed.request_key = "other-request".into(),
            2 => changed.definition.label = "changed".into(),
            _ => changed.source_context = json!({"world":"other-source"}),
        }
        assert!(m.reserve_fork(changed).is_err());
    }
    assert!(m.start_async(&child).is_err());
    assert_eq!(m.reserve_fork(original).unwrap(), reservation);
}
#[test]
fn fork_restore_gate_confirmation_fault_and_cold_reopen_do_not_replay_inputs() {
    let f = Fixture::new();
    let mut m = f.manager();
    let original = fork_request(&mut m, &f);
    let r = m.reserve_fork(original.clone()).unwrap();
    let child = r.child_world_id.clone();
    let digest = r.lineage_sha256().unwrap();
    assert!(m.confirm_fork_lineage(r.clone(), digest.clone()).is_err());
    m.restore_fork_async(r.clone(), 0, original.checkpoint_bytes.clone())
        .unwrap();
    let running = await_fork(&mut m, &child);
    assert_eq!(running["external_publication"]["fork"]["ready"], false);
    let after_restore = commits(&f);
    m.restore_fork_async(r.clone(), 0, original.checkpoint_bytes.clone())
        .unwrap();
    assert_eq!(commits(&f), after_restore);
    let input = request(
        &child,
        1,
        running["revision"].as_u64().unwrap(),
        "child-one",
        Some(&original.published.receipt_sha256),
    );
    assert!(m
        .admit_boundary_async(serde_json::from_value(input.clone()).unwrap())
        .is_err());
    assert!(m.confirm_fork_lineage(r.clone(), "0".repeat(64)).is_err());
    let target = f.root.join("worlds").join(&child).join("world.json");
    let backup = target.with_extension("saved");
    fs::rename(&target, &backup).unwrap();
    fs::create_dir(&target).unwrap();
    assert!(m.confirm_fork_lineage(r.clone(), digest.clone()).is_err());
    assert!(m
        .admit_boundary_async(serde_json::from_value(input.clone()).unwrap())
        .is_err());
    fs::remove_dir(&target).unwrap();
    fs::rename(&backup, &target).unwrap();
    drop(m);
    let mut m = f.manager();
    assert_eq!(m.reserve_fork(original.clone()).unwrap(), r);
    assert_eq!(
        m.status(&child).unwrap()["external_publication"]["fork"]["ready"],
        false
    );
    m.restore_fork_async(r.clone(), 1, original.checkpoint_bytes.clone())
        .unwrap();
    let status = await_fork(&mut m, &child);
    m.confirm_fork_lineage(r.clone(), digest.clone()).unwrap();
    m.confirm_fork_lineage(r.clone(), digest.clone()).unwrap();
    let k = key(&admit(
        &mut m,
        request(
            &child,
            2,
            status["revision"].as_u64().unwrap(),
            "child-one",
            Some(&original.published.receipt_sha256),
        ),
    ));
    let frozen = finished(&mut m, &k);
    assert_eq!(
        frozen["boundary"]["manifest"]["binding"]["parent_receipt_sha256"],
        original.published.receipt_sha256
    );
    assert!(m
        .restore_fork_async(r.clone(), 2, original.checkpoint_bytes.clone())
        .is_err());
    m.confirm_fork_lineage(r.clone(), digest.clone()).unwrap();
    assert!(m.start_async(&child).is_err());
    let before = commits(&f);
    drop(m);
    let mut m = f.manager();
    assert_eq!(m.reserve_fork(original).unwrap(), r);
    m.confirm_fork_lineage(r, digest).unwrap();
    assert_eq!(lookup(&mut m, &k)["state"], "frozen");
    assert_eq!(commits(&f), before);
}
#[test]
fn missing_entire_fork_catalog_fails_closed() {
    let f = Fixture::new();
    let mut m = f.manager();
    let original = fork_request(&mut m, &f);
    m.reserve_fork(original).unwrap();
    drop(m);
    fs::rename(f.root.join("worlds/forks"), f.root.join("retained-forks")).unwrap();
    let error = WorldManager::new(
        f.root.join("registry"),
        f.root.join("worlds"),
        f.root.join("build.py"),
    )
    .err()
    .unwrap();
    assert!(error.contains("no reservation catalog"), "{error}");
}
#[test]
fn lost_fork_world_record_never_recreates_a_progressed_child() {
    let f = Fixture::new();
    let mut m = f.manager();
    let request = fork_request(&mut m, &f);
    let reserved = m.reserve_fork(request.clone()).unwrap();
    let child = reserved.child_world_id.clone();
    m.restore_fork_async(reserved.clone(), 0, request.checkpoint_bytes.clone())
        .unwrap();
    let live = await_fork(&mut m, &child);
    m.confirm_fork_lineage(reserved.clone(), reserved.lineage_sha256().unwrap())
        .unwrap();
    let k = key(&admit(
        &mut m,
        self::request(
            &child,
            1,
            live["revision"].as_u64().unwrap(),
            "child-progress",
            Some(&request.published.receipt_sha256),
        ),
    ));
    assert_eq!(finished(&mut m, &k)["state"], "frozen");
    drop(m);
    let dir = f.root.join("worlds").join(&child);
    let saved = fs::read(dir.join("world.json")).unwrap();
    fs::remove_file(dir.join("world.json")).unwrap();
    let before = fs::read(f.root.join("commands")).unwrap();
    let error = WorldManager::new(
        f.root.join("registry"),
        f.root.join("worlds"),
        f.root.join("build.py"),
    )
    .err()
    .expect("missing progressed control record must fail closed");
    assert!(
        error.to_lowercase().contains("materialized") || error.contains("progress"),
        "{error}"
    );
    assert!(!dir.join("world.json").exists());
    assert_eq!(fs::read(f.root.join("commands")).unwrap(), before);
    // A whole missing child directory is equally corrupt once birth completed.
    let retained = f.root.join("retained-child");
    fs::rename(&dir, &retained).unwrap();
    assert!(WorldManager::new(
        f.root.join("registry"),
        f.root.join("worlds"),
        f.root.join("build.py")
    )
    .is_err());
    assert!(!dir.exists());
    fs::rename(&retained, &dir).unwrap();
    fs::write(dir.join("world.json"), saved).unwrap();
    let mut m = f.manager();
    assert_eq!(lookup(&mut m, &k)["state"], "frozen");
    assert!(m
        .restore_fork_async(reserved, 1, request.checkpoint_bytes)
        .is_err());
}
#[test]
fn fork_materialization_fence_retry_preserves_completed_birth_record() {
    use ddlog_runtime::worlds::ForkFault;
    let f = Fixture::new();
    let mut m = f.manager();
    let original = fork_request(&mut m, &f);
    let before = fs::read(f.root.join("commands")).unwrap();
    for reopen in [false, true] {
        let mut request = original.clone();
        request.request_key = format!("fence-{reopen}");
        request.destination = json!({"child":reopen});
        assert!(m
            .reserve_fork_with_fault(request.clone(), ForkFault::AfterChildRecord)
            .is_err());
        let path = fs::read_dir(f.root.join("worlds/forks"))
            .unwrap()
            .filter_map(|e| {
                let path = e.unwrap().path();
                if path.extension().and_then(|x| x.to_str()) != Some("json") {
                    return None;
                }
                let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                (record["reservation"]["request_key"] == request.request_key).then_some(path)
            })
            .next()
            .unwrap();
        let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let child = record["reservation"]["child_world_id"].as_str().unwrap();
        let child_path = f.root.join("worlds").join(child).join("world.json");
        let saved = fs::read(&child_path).unwrap();
        assert!(!path.with_extension("materialized").exists());
        assert!(m.status(child).is_err());
        if reopen {
            drop(m);
            m = f.manager();
        }
        let result = m.reserve_fork(request).unwrap();
        assert_eq!(result.child_world_id, child);
        assert_eq!(fs::read(&child_path).unwrap(), saved);
        assert!(path.with_extension("materialized").exists());
        assert_eq!(m.status(child).unwrap()["generation"], 0);
        assert!(m.start_async(child).is_err());
    }
    assert_eq!(fs::read(f.root.join("commands")).unwrap(), before);
}

#[test]
fn fork_rename_success_directory_sync_failure_repeats_the_child_fence() {
    use ddlog_runtime::worlds::ForkFault;
    let f = Fixture::new();
    let mut m = f.manager();
    let request = fork_request(&mut m, &f);
    let before = fs::read(f.root.join("commands")).unwrap();
    assert!(m
        .reserve_fork_with_fault(request.clone(), ForkFault::ChildDirectorySync)
        .unwrap_err()
        .contains("Injected child directory sync failure"));
    let path = fs::read_dir(f.root.join("worlds/forks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let child = record["reservation"]["child_world_id"].as_str().unwrap();
    let child_path = f.root.join("worlds").join(child).join("world.json");
    let saved = fs::read(&child_path).unwrap();
    let marker = path.with_extension("materialized");
    assert!(!marker.exists());
    assert!(m.status(child).is_err());
    // A retained rename must not bypass the failed directory fence on retry.
    assert!(m
        .reserve_fork_with_fault(request.clone(), ForkFault::ChildDirectorySync)
        .expect_err("retry must re-fence the retained child record")
        .contains("Injected child directory sync failure"));
    assert_eq!(fs::read(&child_path).unwrap(), saved);
    assert!(!marker.exists());
    assert!(m.status(child).is_err());
    let reservation = m.reserve_fork(request.clone()).unwrap();
    assert_eq!(reservation.child_world_id, child);
    assert_eq!(fs::read(&child_path).unwrap(), saved);
    assert!(marker.exists());
    // An in-memory child and existing marker must also repeat this fence.
    assert!(m
        .reserve_fork_with_fault(request.clone(), ForkFault::ChildDirectorySync)
        .is_err());
    assert_eq!(fs::read(&child_path).unwrap(), saved);
    drop(m);
    let mut m = f.manager();
    assert_eq!(m.reserve_fork(request).unwrap(), reservation);
    assert_eq!(fs::read(&child_path).unwrap(), saved);
    assert_eq!(m.status(child).unwrap()["generation"], 0);
    assert!(m.start_async(child).is_err());
    assert_eq!(fs::read(f.root.join("commands")).unwrap(), before);
}
