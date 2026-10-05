//! Strict live Bool/finite Float64 contract; actual compiler acceptance is opt-in.
use ddlog_runtime::{lower, rows::decode_rows, Schema};
use serde_json::json;
use std::collections::BTreeMap;

fn schemas() -> BTreeMap<String, Schema> {
    serde_json::from_value(json!({
        "seed":{"input":true,"fields":["int","bool","double"]},
        "result":{"input":false,"fields":["int","bool","double"]}
    }))
    .unwrap()
}

#[test]
fn real_native_schema_and_typed_constants_are_lowered() {
    let source = lower("result(E,false,-1.25e2) :- seed(E,true,D).", &schemas()).unwrap();
    assert!(source.contains("f1: bool, f2: double"), "{source}");
    assert!(source.contains("false, -64'f125.0"), "{source}");
    assert!(source.contains("v_D: double"), "{source}");
    for invalid in [
        "result(E,true,1) :- seed(E,true,D).",
        "result(E,1,1.0) :- seed(E,true,D).",
        "result(E,false,1e400) :- seed(E,true,D).",
        "result(E,false,1e-400) :- seed(E,true,D).",
    ] {
        assert!(lower(invalid, &schemas()).is_err(), "{invalid}");
    }
}

#[test]
fn boolean_predicate_names_and_legacy_string_symbols_keep_their_meaning() {
    let schema = serde_json::from_value(json!({
        "true":{"input":true,"fields":["int","string"]},
        "false":{"input":true,"fields":["int","string"]},
        "result":{"input":false,"fields":["int","string"]}
    }))
    .unwrap();
    let source = lower(
        "result(E,false) :- true(E,true). result(E,S) :- false(E,S), S = true.",
        &schema,
    )
    .unwrap();
    assert!(source.contains("R_true(v_E, \"true\")"), "{source}");
    assert!(source.contains("R_result(v_E, \"false\")"), "{source}");
    assert!(source.contains("v_S == \"true\""), "{source}");
    let old = lower(
        "result(E,S) :- true(E,S), true = \"true\", false \\= other, true \\= false.",
        &schema,
    )
    .unwrap();
    let quoted = lower("result(E,S) :- true(E,S), \"true\" = \"true\", \"false\" \\= other, \"true\" \\= \"false\".", &schema).unwrap();
    assert_eq!(old, quoted);
    let program = serde_json::from_value(
        json!({"rules":"result(E,false) :- true(E,true), true = \"true\".", "schemas":schema}),
    )
    .unwrap();
    let view = ddlog_runtime::source_inspection::inspect_program(&program);
    assert_eq!(
        view["rules"][0]["head"]["arguments"][1]["kind"], "string",
        "{view}"
    );
    assert_eq!(
        view["rules"][0]["conditions"][0]["atom"]["arguments"][1]["kind"], "string",
        "{view}"
    );
}

#[test]
fn schema_directed_native_snapshot_preserves_binary64_and_normalizes_zero() {
    let fields = schemas()["result"].fields.clone();
    let values = [
        0.0,
        -0.0,
        1.0,
        f64::from_bits(1.0f64.to_bits() + 1),
        f64::MAX,
        f64::from_bits(1),
        -f64::from_bits(1),
    ];
    for value in values {
        // Native Double display can spell an integral double without a decimal point.
        let text = format!("R_result{{.f0 = -9223372036854775808, .f1 = true, .f2 = {value}}}");
        let decoded = decode_rows(&text, "result", &fields).unwrap();
        assert_eq!(decoded[0][0], json!(i64::MIN));
        assert_eq!(decoded[0][1], json!(true));
        let actual = decoded[0][2].as_f64().unwrap();
        assert_eq!(
            actual.to_bits(),
            if value == 0.0 { 0 } else { value.to_bits() },
            "{text}"
        );
        assert!(decoded[0][2].is_f64());
    }
    for value in ["NaN", "inf", "-inf", "1e400", "1e-400", "true", "\"1.0\""] {
        assert!(
            decode_rows(
                &format!("R_result{{.f0 = 1, .f1 = true, .f2 = {value}}}"),
                "result",
                &fields
            )
            .is_err(),
            "{value}"
        );
    }
    for value in ["0", "1", "\"true\"", "True"] {
        assert!(
            decode_rows(
                &format!("R_result{{.f0 = 1, .f1 = {value}, .f2 = 1.0}}"),
                "result",
                &fields
            )
            .is_err(),
            "{value}"
        );
    }
}

#[test]
#[ignore = "requires DDLOG_RUNTIME_NATIVE_BUILD and real DDlog Bool/Double compiler"]
fn actual_native_composition_values_checkpoint_and_opposite_zero_delete() {
    use ddlog_runtime::{registry::ProcessorRegistry, Backend};
    let root = std::env::temp_dir().join(format!(
        "live-values-native-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let driver = std::path::PathBuf::from(
        std::env::var_os("DDLOG_RUNTIME_NATIVE_BUILD")
            .expect("Configure actual native build driver"),
    );
    let registry = ProcessorRegistry::open(root.join("registry")).unwrap();
    let first = registry.create(serde_json::from_value(json!({"rules":"out(E,B,D) :- seed(E,B,D).",
        "schemas":{"seed":{"input":true,"fields":["int","bool","double"]}, "out":{"input":false,"fields":["int","bool","double"]}},
        "interface":{"inputs":["seed"],"outputs":["out"]}})).unwrap(),None).unwrap();
    let second = registry.create(serde_json::from_value(json!({"rules":"result(E,B,D) :- source(E,B,D). constants(E,false,-1.25e2) :- source(E,true,D).",
        "schemas":{"source":{"input":true,"fields":["int","bool","double"]}, "result":{"input":false,"fields":["int","bool","double"]}, "constants":{"input":false,"fields":["int","bool","double"]}},
        "interface":{"inputs":["source"],"outputs":["result","constants"]}})).unwrap(),None).unwrap();
    let manifest = serde_json::from_value(json!({"nodes":{"first":{"processor_id":first.processor_id,"version":first.version},"second":{"processor_id":second.processor_id,"version":second.version}},
        "inputs":{"seed":{"fields":["int","bool","double"],"targets":[{"node":"first","relation":"seed"}]}},
        "bindings":[{"from":{"node":"first","relation":"out"},"to":{"node":"second","relation":"source"}}],
        "outputs":{"result":{"node":"second","relation":"result"},"constants":{"node":"second","relation":"constants"}}})).unwrap();
    let mut backend = Backend::new(root.join("live"), driver.clone());
    backend.set_lowering_version(2).unwrap();
    let resolution = backend.install_composition(&registry, &manifest).unwrap();
    let input = &resolution.inputs["seed"];
    let output = &resolution.outputs["result"];
    let constants = &resolution.outputs["constants"];
    let change = |op: &str, entity: i64, value: serde_json::Value| json!({"op":op,"predicate":input,"values":[entity,true,value]});
    backend
        .apply_without_deltas(&json!([change("insert", 1, json!(-0.0))]))
        .unwrap();
    let zero = backend.query_typed(output).unwrap();
    assert_eq!(zero[0][2].as_f64().unwrap().to_bits(), 0);
    assert_eq!(
        backend.export_inputs().unwrap()[input][0][2]
            .as_f64()
            .unwrap()
            .to_bits(),
        0
    );
    backend
        .apply_without_deltas(&json!([change("delete", 1, json!(0.0))]))
        .unwrap();
    assert!(backend.query_typed(output).unwrap().is_empty());
    backend
        .apply_without_deltas(&json!([change("insert", 1, json!(0.0))]))
        .unwrap();
    backend
        .apply_without_deltas(&json!([change("delete", 1, json!(-0.0))]))
        .unwrap();
    assert!(backend.query_typed(output).unwrap().is_empty());
    let values = [
        1.0,
        f64::from_bits(1.0f64.to_bits() + 1),
        f64::MAX,
        -f64::MAX,
        f64::from_bits(1),
        -f64::from_bits(1),
        -125.0,
    ];
    backend
        .apply_without_deltas(&json!(values
            .iter()
            .enumerate()
            .map(|(i, v)| change("insert", i as i64 + 2, json!(*v)))
            .collect::<Vec<_>>()))
        .unwrap();
    backend
        .apply_without_deltas(
            &json!([{"op":"insert","predicate":input,"values":[99,false,-125.0]}]),
        )
        .unwrap();
    let mut expected: BTreeMap<i64, u64> = values
        .iter()
        .enumerate()
        .map(|(i, v)| (i as i64 + 2, v.to_bits()))
        .collect();
    expected.insert(99, (-125.0f64).to_bits());
    let check =
        |backend: &mut Backend| {
            let actual: BTreeMap<i64, u64> = backend
                .query_typed(output)
                .unwrap()
                .into_iter()
                .map(|r| {
                    assert_eq!(r[1], json!(r[0].as_i64().unwrap() != 99));
                    (r[0].as_i64().unwrap(), r[2].as_f64().unwrap().to_bits())
                })
                .collect();
            assert_eq!(actual, expected);
            let constant = backend.query_typed(constants).unwrap();
            assert_eq!(constant.len(), values.len());
            assert!(constant.iter().all(|r| r[1] == json!(false)
                && r[2].as_f64().unwrap().to_bits() == (-125.0f64).to_bits()));
        };
    check(&mut backend);
    let revision = backend.revision();
    for invalid in [json!(1), json!(true), json!("1.0"), serde_json::Value::Null] {
        assert!(backend
            .apply_without_deltas(&json!([change("insert", 99, invalid)]))
            .is_err());
        assert_eq!(backend.revision(), revision);
    }
    let checkpoint = backend
        .checkpoint_bytes(json!({"contract":"live_values"}))
        .unwrap();
    drop(backend);
    let mut restored = Backend::new(root.join("restored"), driver);
    assert_eq!(
        restored.restore_checkpoint_bytes(&checkpoint).unwrap(),
        json!({"contract":"live_values"})
    );
    check(&mut restored);
    // Recomputed digests cannot make malformed typed recovery state valid.
    for invalid in [json!(1), json!(-0.0)] {
        use sha2::{Digest, Sha256};
        let mut forged: serde_json::Value = serde_json::from_slice(&checkpoint).unwrap();
        let rows = forged["state"]["inputs"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .find(|v| v.as_array().is_some_and(|v| !v.is_empty()))
            .unwrap();
        rows[0][2] = invalid;
        forged["sha256"] = json!(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&forged["state"]).unwrap())
        ));
        let revision = restored.revision();
        assert!(restored
            .restore_checkpoint_bytes(&serde_json::to_vec(&forged).unwrap())
            .is_err());
        assert_eq!(restored.revision(), revision);
        check(&mut restored);
    }
    restored
        .apply_without_deltas(&json!(values
            .iter()
            .enumerate()
            .map(|(i, v)| change("delete", i as i64 + 2, json!(*v)))
            .collect::<Vec<_>>()))
        .unwrap();
    restored
        .apply_without_deltas(
            &json!([{"op":"delete","predicate":input,"values":[99,false,-125.0]}]),
        )
        .unwrap();
    assert!(restored.query_typed(output).unwrap().is_empty());
    assert!(restored.query_typed(constants).unwrap().is_empty());
    println!(
        "ACTUAL DDlog Bool/Double composition/checkpoint/retraction evidence: {}",
        root.display()
    );
}
