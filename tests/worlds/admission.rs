//! Fences and failure classification with a simulated native transport.
use super::*;
fn admit(manager: &mut WorldManager, request: Value) -> Value {
    manager
        .admit_inputs(serde_json::from_value(request).unwrap())
        .unwrap()
}
fn request(id: &str, generation: u64, revision: u64, key: &str) -> Value {
    json!({"id":id,"expected_generation":generation,"expected_revision":revision,"admission_key":key,
        "changes":[{"op":"insert","predicate":"source","values":[revision,"private input"]}]})
}
fn read(
    manager: &mut WorldManager,
    id: &str,
    generation: u64,
    revision: u64,
) -> Result<Value, String> {
    manager.read_batch(
        serde_json::from_value(
            json!({"id":id,"expected_generation":generation,"expected_revision":revision,
        "queries":[{"predicate":"source","max_rows":1},{"predicate":"echo"}]}),
        )
        .unwrap(),
    )
}
#[test]
fn cas_reads_effects_replay_and_restore_fence_all_mutations() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    manager.start(&id).unwrap();
    let initial = read(&mut manager, &id, 1, 1).unwrap();
    assert_eq!(initial["results"][0]["rows"], json!([]));
    let mut claim = request(&id, 1, 1, "claim");
    claim["effect"] = json!({"key":"effect","phase":"reserve"});
    let reserved = admit(&mut manager, claim.clone());
    assert_eq!(reserved["state"], "durable");
    assert_eq!(reserved["effect_authorized"], true);
    assert_eq!(reserved["receipt"]["origin"]["revision"], 2);
    let duplicate = admit(&mut manager, claim.clone());
    assert_eq!(duplicate["receipt"], reserved["receipt"]);
    assert_eq!(duplicate["effect_authorized"], false);
    assert_eq!(duplicate["replayed"], true);
    claim["changes"][0]["values"][0] = json!(99);
    assert!(manager
        .admit_inputs(serde_json::from_value(claim).unwrap())
        .is_err());
    assert!(read(&mut manager, &id, 1, 1).is_err());
    let stale = admit(&mut manager, request(&id, 1, 1, "stale"));
    assert_eq!(stale["state"], "not_applied");
    let mut second = request(&id, 1, 2, "second-claim");
    second["effect"] = json!({"key":"effect","phase":"reserve"});
    assert_eq!(admit(&mut manager, second)["state"], "not_applied");
    let mut settle = request(&id, 1, 2, "settle");
    settle["effect"] = json!({"key":"effect","phase":"settle","reservation_key":"wrong"});
    assert_eq!(admit(&mut manager, settle.clone())["state"], "not_applied");
    settle["effect"]["reservation_key"] = json!("claim");
    assert_eq!(admit(&mut manager, settle.clone())["state"], "durable");
    settle["admission_key"] = json!("duplicate-settlement");
    settle["expected_revision"] = json!(3);
    assert_eq!(admit(&mut manager, settle)["state"], "not_applied");
    let page = read(&mut manager, &id, 1, 3).unwrap();
    assert_eq!(page["results"][0]["total"], 2);
    assert_eq!(page["results"][0]["complete"], false);
    assert_eq!(page["results"][1]["revision"], 3);
    assert_eq!(
        admit(&mut manager, request(&id, 1, 3, "advance"))["state"],
        "durable"
    );
    assert!(manager
        .read_batch(
            serde_json::from_value(
                json!({"id":id,"expected_generation":1,"expected_revision":4,
        "queries":[{"predicate":"source","continuation":page["results"][0]["continuation"]}]})
            )
            .unwrap()
        )
        .is_err());
    let commands = fs::read_to_string(f.root.join("commands")).unwrap();
    assert!(
        !commands.contains("commit dump_changes"),
        "admission must never consume unbounded deltas"
    );
    manager.stop(&id).unwrap();
    drop(manager);
    let mut manager = f.manager();
    let historical = manager
        .admission_status(
            serde_json::from_value(json!({"id":id,"generation":1,"admission_key":"claim"}))
                .unwrap(),
        )
        .unwrap();
    assert_eq!(historical["receipt"], reserved["receipt"]);
    assert_eq!(historical["effect_authorized"], false);
    manager.restore_async(&id, &reserved["receipt"]).unwrap();
    wait_until(&mut manager, &id, |s| s["state"] == "running");
    let mut retry = request(&id, 2, 2, "new-claim");
    retry["effect"] = json!({"key":"effect","phase":"reserve"});
    assert_eq!(admit(&mut manager, retry.clone())["state"], "not_applied");
    retry["effect"] = json!({"key":"effect","phase":"settle","reservation_key":"claim"});
    assert_eq!(admit(&mut manager, retry)["state"], "not_applied");
    manager.stop(&id).unwrap();
    manager.start(&id).unwrap();
    let mut fresh = request(&id, 3, 1, "fresh");
    fresh["effect"] = json!({"key":"effect","phase":"reserve"});
    assert_eq!(admit(&mut manager, fresh)["effect_authorized"], true);
}
#[test]
fn publication_and_native_ack_failures_never_authorize_effects() {
    for lose_ack in [false, true] {
        let f = Fixture::new();
        let mut manager = f.manager();
        let def = definition(&manager);
        let id = manager.create(def).unwrap();
        manager.start(&id).unwrap();
        if lose_ack {
            fs::write(f.root.join("die_on_commit"), "").unwrap();
        } else {
            fs::write(
                f.root.join("worlds").join(&id).join("checkpoints"),
                "block publication",
            )
            .unwrap();
        }
        let mut claim = request(&id, 1, 1, "claim");
        claim["effect"] = json!({"key":"effect","phase":"reserve"});
        let failed = admit(&mut manager, claim.clone());
        assert_eq!(
            failed["state"],
            if lose_ack {
                "uncertain"
            } else {
                "applied_but_unpublished"
            }
        );
        assert_eq!(failed["receipt"], Value::Null);
        assert_eq!(failed["effect_authorized"], false);
        assert_eq!(
            failed["applied_revision"],
            if lose_ack { Value::Null } else { json!(2) }
        );
        assert_eq!(admit(&mut manager, claim)["effect_authorized"], false);
        if !lose_ack {
            assert_eq!(
                read(&mut manager, &id, 1, 2).unwrap()["results"][0]["total"],
                1
            );
            fs::remove_file(f.root.join("worlds").join(&id).join("checkpoints")).unwrap();
            let mut settle = request(&id, 1, 2, "settle");
            settle["effect"] = json!({"key":"effect","phase":"settle","reservation_key":"claim"});
            assert_eq!(admit(&mut manager, settle)["state"], "not_applied");
        }
        drop(manager);
        let mut manager = f.manager();
        let persisted = manager
            .admission_status(
                serde_json::from_value(json!({"id":id,"generation":1,"admission_key":"claim"}))
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(persisted["state"], failed["state"]);
        assert_eq!(persisted["effect_authorized"], false);
    }
}
#[test]
fn input_validation_limits_and_intent_persistence_precede_native_commit() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    manager.start(&id).unwrap();
    let before = fs::read(f.root.join("commands")).unwrap();
    let mut invalid = request(&id, 1, 1, "bad");
    invalid["changes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"op":"insert","predicate":"echo","values":[2,"derived"]}));
    assert_eq!(admit(&mut manager, invalid)["state"], "not_applied");
    assert_eq!(fs::read(f.root.join("commands")).unwrap(), before);
    let queries = vec![json!({"predicate":"source","max_rows":100}); 11];
    assert!(manager
        .read_batch(
            serde_json::from_value(
                json!({"id":id,"expected_generation":1,"expected_revision":1,"queries":queries})
            )
            .unwrap()
        )
        .is_err());
    // Block atomic replacement of world.json, but not status: a clean status
    // performs no write. No native mutation may precede durable intent.
    let path = f.root.join("worlds").join(&id).join("world.json");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let failed = admit(&mut manager, request(&id, 1, 1, "intent-failure"));
    assert_eq!(failed["state"], "not_applied");
    assert_eq!(fs::read(f.root.join("commands")).unwrap(), before);
    fs::remove_dir(path).unwrap();
}
#[test]
fn input_page_byte_limit_and_crashed_intents_are_honest() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let def = definition(&manager);
    let id = manager.create(def).unwrap();
    manager.start(&id).unwrap();
    admit(&mut manager, request(&id, 1, 1, "one"));
    let page = manager
        .execute(
            &id,
            "query_rows",
            &json!({"predicate":"source","max_bytes":64}),
        )
        .unwrap();
    assert_eq!(page["rows"].as_array().unwrap().len(), 1);
    assert!(manager
        .execute(
            &id,
            "query_rows",
            &json!({"predicate":"source","max_bytes":2})
        )
        .is_err());
    drop(manager);
    let path = f.root.join("worlds").join(&id).join("world.json");
    let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["admission"]["records"]["1/one"]["state"] = json!("pending");
    fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
    let mut manager = f.manager();
    let result = manager
        .admission_status(
            serde_json::from_value(json!({"id":id,"generation":1,"admission_key":"one"})).unwrap(),
        )
        .unwrap();
    assert_eq!(result["state"], "uncertain");
    assert_eq!(result["receipt"], Value::Null);
}
