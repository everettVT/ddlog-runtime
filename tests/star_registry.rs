#![cfg(unix)]

use ddlog_runtime::composition::CompositionManifest;
use ddlog_runtime::registry::{
    ProcessorDefinition, ProcessorReference, ProcessorRegistry, ProcessorVersion,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
struct TestDirectory(PathBuf);
impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "lemmalog-star-registry-test-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn registry(&self) -> ProcessorRegistry {
        ProcessorRegistry::open(self.0.join("registry")).unwrap()
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn definition(value: Value) -> ProcessorDefinition {
    serde_json::from_value(value).unwrap()
}
fn program() -> Value {
    json!({
        "rules":"",
        "schemas":{
            "vertices":{"input":true,"fields":["int"]},
            "edges":{"input":true,"fields":["int","int"]},
            "labels":{"input":false,"fields":["int","int"]}
        },
        "operators":[{"type":"large_small_star","vertices":"vertices","edges":"edges","output":"labels"}],
        "interface":{"inputs":["vertices","edges"],"outputs":["labels"]}
    })
}
fn reference(record: &ProcessorVersion) -> ProcessorReference {
    ProcessorReference {
        processor_id: record.processor_id.clone(),
        version: record.version.clone(),
    }
}
fn snapshot(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(snapshot(&path));
        } else {
            result.insert(path.clone(), fs::read(path).unwrap());
        }
    }
    result
}

#[test]
fn empty_operators_preserve_legacy_wire_shape_and_content_hash() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let legacy = json!({"rules":"visible(X) :- item(X).","schemas":{
        "item":{"input":true,"fields":["string"]},
        "visible":{"input":false,"fields":["string"]}},"operation":null});
    let original = registry.create(definition(legacy.clone()), None).unwrap();
    let mut explicit_empty = legacy.clone();
    explicit_empty["operators"] = json!([]);
    let empty = registry.create(definition(explicit_empty), None).unwrap();
    assert_eq!(original.version, empty.version);
    assert_eq!(
        original.content_sha256,
        "0a9c8173319f8107f9ad2c37446de12021176145edc6c7fbe2862dfac4a9c657"
    );
    assert_eq!(serde_json::to_value(empty.definition).unwrap(), legacy);
}

#[test]
fn operator_only_definition_validates_without_native_compilation_and_pins_history() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(program()), None).unwrap();
    assert!(first.validation.syntax_checked && first.validation.types_checked);
    assert!(first.validation.supported_lowering_checked);
    assert!(!first.validation.ddlog_compilation_performed);
    assert_eq!(
        serde_json::to_value(&first.definition).unwrap()["operators"],
        program()["operators"]
    );
    let mut changed = program();
    changed["operators"][0]["edges"] = json!("alternate_edges");
    changed["schemas"]["alternate_edges"] = json!({"input":true,"fields":["int","int"]});
    changed["interface"]["inputs"] = json!(["vertices", "edges", "alternate_edges"]);
    let second = registry
        .publish(
            &first.processor_id,
            definition(changed),
            &first.version,
            None,
        )
        .unwrap();
    assert_ne!(first.version, second.version);
    assert_eq!(
        registry
            .get(&first.processor_id, Some(&first.version))
            .unwrap(),
        first
    );
    assert_eq!(registry.get(&first.processor_id, None).unwrap(), second);
    let fork = registry
        .fork(&first.processor_id, &first.version, None)
        .unwrap();
    assert_eq!(fork.definition, first.definition);
    assert_eq!(fork.lineage, Some(reference(&first)));
    assert_eq!(fork.version, first.version);
    assert!(snapshot(&directory.0).keys().all(|path| path
        .extension()
        .is_some_and(|extension| extension == "json")));
}

#[test]
fn invalid_operator_types_references_and_cycles_never_publish() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(program()), None).unwrap();
    let before = snapshot(&directory.0);
    let mut missing = program();
    missing["operators"][0]["vertices"] = json!("missing");
    let mut wrong_type = program();
    wrong_type["schemas"]["edges"]["fields"] = json!(["string", "string"]);
    let mut wrong_arity = program();
    wrong_arity["schemas"]["vertices"]["fields"] = json!(["int", "int"]);
    let mut input_output = program();
    input_output["schemas"]["labels"]["input"] = json!(true);
    input_output.as_object_mut().unwrap().remove("interface");
    let mut recursive = program();
    recursive["schemas"]["edges"]["input"] = json!(false);
    recursive["rules"] = json!("edges(U,V) :- labels(U,V).");
    recursive["interface"]["inputs"] = json!(["vertices"]);
    for (candidate, cause) in [
        (missing, "missing"),
        (wrong_type, "int"),
        (wrong_arity, "int"),
        (input_output, "input"),
        (recursive, "Recursive"),
    ] {
        assert!(registry
            .create(definition(candidate.clone()), None)
            .unwrap_err()
            .contains(cause));
        assert!(registry
            .publish(
                &first.processor_id,
                definition(candidate),
                &first.version,
                None
            )
            .unwrap_err()
            .contains(cause));
        assert_eq!(snapshot(&directory.0), before);
        assert_eq!(registry.get(&first.processor_id, None).unwrap(), first);
    }
}

#[test]
fn typed_operator_rejects_unknown_fields_and_registered_operation_mixing() {
    let mut unknown_field = program();
    unknown_field["operators"][0]["native_path"] = json!("operator.rs");
    assert!(serde_json::from_value::<ProcessorDefinition>(unknown_field).is_err());
    let mut unknown_type = program();
    unknown_type["operators"][0]["type"] = json!("arbitrary_transformer");
    assert!(serde_json::from_value::<ProcessorDefinition>(unknown_type).is_err());
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let mut mixed = program();
    mixed["operation"] = json!({"name":"review","version":"v1","description":"Review"});
    let error = registry.create(definition(mixed), None).unwrap_err();
    assert!(
        error.contains("operators") && error.contains("registered operation"),
        "{error}"
    );
    assert!(snapshot(&directory.0).is_empty());
}

#[test]
fn nested_composition_namespaces_operators_and_keeps_rule_witness_indexes() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let leaf = registry.create(definition(program()), None).unwrap();
    let inner_manifest = json!({"nodes":{"left":reference(&leaf),"right":reference(&leaf)},
        "inputs":{
            "vertices":{"fields":["int"],"targets":[{"node":"left","relation":"vertices"},{"node":"right","relation":"vertices"}]},
            "edges":{"fields":["int","int"],"targets":[{"node":"left","relation":"edges"},{"node":"right","relation":"edges"}]}},
        "bindings":[],"outputs":{"left_labels":{"node":"left","relation":"labels"},"right_labels":{"node":"right","relation":"labels"}}});
    let inner = registry
        .create(definition(json!({"composition":inner_manifest})), None)
        .unwrap();
    let wrapper: CompositionManifest = serde_json::from_value(json!({"nodes":{"nested":reference(&inner)},
        "inputs":{
            "vertices":{"fields":["int"],"targets":[{"node":"nested","relation":"vertices"}]},
            "edges":{"fields":["int","int"],"targets":[{"node":"nested","relation":"edges"}]}},
        "bindings":[],"outputs":{"left_labels":{"node":"nested","relation":"left_labels"},"right_labels":{"node":"nested","relation":"right_labels"}}})).unwrap();
    let outer = registry
        .create(definition(json!({"composition":wrapper})), None)
        .unwrap();
    let compiled = registry.compile_composition(&wrapper).unwrap();
    assert_eq!(compiled.resolution, outer.composition.clone().unwrap());
    assert_eq!(
        compiled
            .source
            .matches("apply lemmalog_star::LargeSmallStar(")
            .count(),
        2
    );
    let mut physical_outputs = Vec::new();
    for (index, path) in ["nested.left", "nested.right"].iter().enumerate() {
        assert_eq!(compiled.resolution.dependencies[*path], reference(&leaf));
        let names: BTreeMap<_, _> = compiled
            .resolution
            .relations
            .iter()
            .filter(|(_, origin)| origin["node"] == *path && origin["kind"] == "processor_relation")
            .map(|(physical, origin)| (origin["relation"].as_str().unwrap(), physical))
            .collect();
        assert!(compiled.source.contains(&format!(
            "LargeSmallStar(R_{}, star_vertex{index}, R_{},",
            names["vertices"], names["edges"]
        )));
        assert!(compiled.source.contains(&format!(
            "R_{}(v, label) :- StarResult{index}[(v, label)].",
            names["labels"]
        )));
        physical_outputs.push(names["labels"].clone());
    }
    assert_ne!(physical_outputs[0], physical_outputs[1]);
    assert_eq!(compiled.resolution.rules.len(), 10);
    assert_eq!(
        compiled.source.matches("output relation Evidence").count(),
        compiled.resolution.rules.len()
    );
    for index in 0..compiled.resolution.rules.len() {
        assert!(compiled
            .source
            .contains(&format!("output relation Evidence{index}(")));
    }
    assert!(compiled
        .resolution
        .rules
        .iter()
        .all(|origin| origin["kind"] != "processor_rule"));
    let mut changed = program();
    changed["rules"] = json!("\n");
    registry
        .publish(&leaf.processor_id, definition(changed), &leaf.version, None)
        .unwrap();
    assert_eq!(
        registry.compile_composition(&wrapper).unwrap().source,
        compiled.source
    );
    assert_eq!(registry.get(&outer.processor_id, None).unwrap(), outer);
}
