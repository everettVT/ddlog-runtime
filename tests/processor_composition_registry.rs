#![cfg(unix)]

use ddlog_runtime::composition::CompositionManifest;
use ddlog_runtime::registry::{
    CompositionDefinition, ProcessorDefinition, ProcessorReference, ProcessorRegistry,
    ProcessorVersion,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
struct TestDirectory(PathBuf);
impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "lemmalog-composition-registry-test-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn registry(&self) -> ProcessorRegistry {
        ProcessorRegistry::open(self.0.join("registry")).unwrap()
    }
    fn record_path(&self, record: &ProcessorVersion) -> PathBuf {
        self.0
            .join("registry")
            .join(&record.processor_id)
            .join("versions")
            .join(format!("{}.json", record.content_sha256))
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn leaf_json() -> Value {
    json!({
        "rules":"middle(X) :- source(X). result(X) :- middle(X).",
        "schemas": {
            "source":{"input":true,"fields":["string"]},
            "middle":{"input":false,"fields":["string"]},
            "result":{"input":false,"fields":["string"]}
        },
        "interface":{"inputs":["source"],"outputs":["result"]}
    })
}
fn definition(value: Value) -> ProcessorDefinition {
    serde_json::from_value(value).unwrap()
}
fn reference(record: &ProcessorVersion) -> ProcessorReference {
    ProcessorReference {
        processor_id: record.processor_id.clone(),
        version: record.version.clone(),
    }
}
fn manifest(leaf: &ProcessorVersion) -> CompositionManifest {
    serde_json::from_value(json!({
        "nodes":{"first":reference(leaf),"second":reference(leaf)},
        "inputs":{"input":{"fields":["string"],"targets":[{"node":"first","relation":"source"}]}},
        "bindings":[{"from":{"node":"first","relation":"result"},"to":{"node":"second","relation":"source"}}],
        "outputs":{"output":{"node":"second","relation":"result"}}
    })).unwrap()
}
fn composed(manifest: CompositionManifest) -> ProcessorDefinition {
    ProcessorDefinition::Composition(CompositionDefinition {
        composition: manifest,
    })
}
fn wrapper_manifest(program: &ProcessorVersion) -> CompositionManifest {
    serde_json::from_value(json!({
        "nodes":{"nested":reference(program)},
        "inputs":{"wrapper_input":{"fields":["string"],"targets":[{"node":"nested","relation":"input"}]}},
        "bindings":[],
        "outputs":{"wrapper_output":{"node":"nested","relation":"output"}}
    })).unwrap()
}
fn snapshot(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            entries.insert(path.clone(), Vec::new());
            entries.extend(snapshot(&path));
        } else {
            entries.insert(path.clone(), fs::read(path).unwrap());
        }
    }
    entries
}

#[test]
fn exact_leaf_versions_resolve_before_publication_and_survive_pointer_moves() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let leaf = registry.create(definition(leaf_json()), None).unwrap();
    let initial_manifest = manifest(&leaf);
    let composition = registry
        .create(composed(initial_manifest.clone()), None)
        .unwrap();
    let resolution = composition.composition.as_ref().unwrap();
    assert_eq!(resolution.nodes, initial_manifest.nodes);
    assert_eq!(resolution.inputs.len(), 1);
    assert!(resolution.inputs.contains_key("input"));
    assert_eq!(resolution.outputs.len(), 1);
    assert!(resolution.outputs.contains_key("output"));
    assert!(!resolution.rules.is_empty());
    assert_eq!(resolution.generated_source_sha256.len(), 64);
    assert!(!composition.validation.ddlog_compilation_performed);
    let compiled = registry.compile_composition(&initial_manifest).unwrap();
    assert_eq!(&compiled.resolution, resolution);
    assert!(!compiled.source.is_empty());
    assert_eq!(
        resolution.generated_source_sha256,
        format!("{:x}", Sha256::digest(compiled.source.as_bytes()))
    );

    let mut next_leaf = leaf_json();
    next_leaf["rules"] = json!("middle(X) :- source(X). result(X) :- middle(X), X = \"included\".");
    let next = registry
        .publish(
            &leaf.processor_id,
            definition(next_leaf),
            &leaf.version,
            None,
        )
        .unwrap();
    assert_ne!(next.version, leaf.version);
    assert_eq!(
        registry.get(&composition.processor_id, None).unwrap(),
        composition
    );
    assert_eq!(
        registry
            .compile_composition(&initial_manifest)
            .unwrap()
            .source,
        compiled.source
    );
    let reopened = directory.registry();
    assert_eq!(
        reopened.get(&composition.processor_id, None).unwrap(),
        composition
    );
}

#[test]
fn composition_hash_cas_and_fork_track_exact_references() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let leaf = registry.create(definition(leaf_json()), None).unwrap();
    let original_manifest = manifest(&leaf);
    let first = registry
        .create(composed(original_manifest.clone()), None)
        .unwrap();
    let identical = registry.create(composed(original_manifest), None).unwrap();
    assert_ne!(first.processor_id, identical.processor_id);
    assert_eq!(first.version, identical.version);
    assert_eq!(first.composition, identical.composition);
    let mut revised_leaf = leaf_json();
    revised_leaf["rules"] = json!("middle(X) :- source(X).\nresult(X) :- middle(X).");
    let revised = registry
        .publish(
            &leaf.processor_id,
            definition(revised_leaf),
            &leaf.version,
            None,
        )
        .unwrap();
    let next_definition = composed(manifest(&revised));
    let second = registry
        .publish(&first.processor_id, next_definition, &first.version, None)
        .unwrap();
    assert_ne!(first.version, second.version);
    assert_ne!(first.composition, second.composition);
    assert!(registry
        .publish(
            &first.processor_id,
            first.definition.clone(),
            &first.version,
            None
        )
        .unwrap_err()
        .contains("Current version conflict"));
    assert_eq!(registry.get(&first.processor_id, None).unwrap(), second);
    assert_eq!(
        registry
            .get(&first.processor_id, Some(&first.version))
            .unwrap(),
        first
    );
    let fork = registry
        .fork(&first.processor_id, &first.version, None)
        .unwrap();
    assert_ne!(fork.processor_id, first.processor_id);
    assert_eq!(fork.version, first.version);
    assert_eq!(fork.composition, first.composition);
    assert_eq!(fork.lineage, Some(reference(&first)));
}

#[test]
fn invalid_composition_never_writes_or_moves_a_pointer() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let leaf = registry.create(definition(leaf_json()), None).unwrap();
    let base = serde_json::to_value(manifest(&leaf)).unwrap();
    let first = registry.create(composed(manifest(&leaf)), None).unwrap();
    let mut invalid = Vec::new();
    let mut missing_version = base.clone();
    missing_version["nodes"]["first"]["version"] = json!(format!("sha256:{}", "0".repeat(64)));
    invalid.push(missing_version);
    let mut private_output = base.clone();
    private_output["bindings"][0]["from"]["relation"] = json!("middle");
    invalid.push(private_output);
    let mut wrong_input_type = base.clone();
    wrong_input_type["inputs"]["input"]["fields"] = json!(["int"]);
    invalid.push(wrong_input_type);
    let mut uncovered_input = base.clone();
    uncovered_input["bindings"] = json!([]);
    invalid.push(uncovered_input);
    let mut cyclic = base.clone();
    cyclic["inputs"] = json!({});
    cyclic["bindings"].as_array_mut().unwrap().push(json!({"from":{"node":"second","relation":"result"},"to":{"node":"first","relation":"source"}}));
    invalid.push(cyclic);
    let before = snapshot(&directory.0.join("registry"));
    for bad in invalid {
        let candidate = composed(serde_json::from_value(bad).unwrap());
        assert!(registry.create(candidate.clone(), None).is_err());
        assert!(registry
            .publish(&first.processor_id, candidate, &first.version, None)
            .is_err());
        assert_eq!(snapshot(&directory.0.join("registry")), before);
        assert_eq!(registry.get(&first.processor_id, None).unwrap(), first);
    }
}

#[test]
fn composed_nodes_are_programs_and_registered_operation_nodes_remain_rejected() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let leaf = registry.create(definition(leaf_json()), None).unwrap();
    let composition = registry.create(composed(manifest(&leaf)), None).unwrap();
    let nested = wrapper_manifest(&composition);
    let wrapper = registry.create(composed(nested.clone()), None).unwrap();
    assert_eq!(wrapper.composition.as_ref().unwrap().nodes, nested.nodes);
    assert!(registry.compile_composition(&nested).is_ok());
    let mut registered = leaf_json();
    registered["operation"] = json!({"name":"review","version":"v1","description":"Review"});
    let registered = registry.create(definition(registered), None).unwrap();
    assert!(registry
        .compile_composition(&manifest(&registered))
        .unwrap_err()
        .contains("Registered operation"));
    // Exact dependency integrity applies through composed program boundaries.
    fs::remove_file(directory.record_path(&leaf)).unwrap();
    assert!(registry
        .compile_composition(&nested)
        .unwrap_err()
        .contains(&leaf.version));
}

#[test]
fn resolution_and_referenced_leaf_integrity_are_verified_on_read() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let leaf = registry.create(definition(leaf_json()), None).unwrap();
    let composition = registry.create(composed(manifest(&leaf)), None).unwrap();
    let path = directory.record_path(&composition);
    let original = fs::read(&path).unwrap();
    let mut record: Value = serde_json::from_slice(&original).unwrap();
    record["composition"]["generated_source_sha256"] = json!("tampered");
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(registry
        .get(&composition.processor_id, None)
        .unwrap_err()
        .contains("resolution mismatch"));
    fs::write(&path, original).unwrap();
    let path = directory.record_path(&leaf);
    let mut leaf_record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    leaf_record["definition"]["rules"] = json!("tampered");
    fs::write(path, serde_json::to_vec(&leaf_record).unwrap()).unwrap();
    assert!(registry
        .get(&composition.processor_id, None)
        .unwrap_err()
        .contains("hash mismatch"));
}

#[test]
fn interface_contract_affects_hash_and_ambiguous_wire_shapes_are_rejected() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let with_interface = leaf_json();
    let mut without_interface = with_interface.clone();
    without_interface
        .as_object_mut()
        .unwrap()
        .remove("interface");
    let legacy = registry
        .create(definition(without_interface), None)
        .unwrap();
    let exported = registry
        .create(definition(with_interface.clone()), None)
        .unwrap();
    assert_ne!(legacy.version, exported.version);
    assert!(legacy.composition.is_none());
    assert!(serde_json::to_value(&legacy.definition)
        .unwrap()
        .get("interface")
        .is_none());
    let mut ambiguous = with_interface;
    ambiguous["composition"] = serde_json::to_value(manifest(&exported)).unwrap();
    assert!(serde_json::from_value::<ProcessorDefinition>(ambiguous).is_err());
    assert!(serde_json::from_value::<ProcessorDefinition>(
        json!({"composition":manifest(&exported),"unknown":true})
    )
    .is_err());
    let mut floating = serde_json::to_value(manifest(&exported)).unwrap();
    floating["nodes"]["first"]
        .as_object_mut()
        .unwrap()
        .remove("version");
    assert!(serde_json::from_value::<CompositionManifest>(floating).is_err());
    assert!(registry.compile_composition(&manifest(&legacy)).is_err());
}

#[test]
fn invalid_program_interfaces_are_rejected_before_registry_writes() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(leaf_json()), None).unwrap();
    let before = snapshot(&directory.0.join("registry"));
    for interface in [
        json!({"inputs":[],"outputs":["result"]}),
        json!({"inputs":["source","source"],"outputs":["result"]}),
        json!({"inputs":["source"],"outputs":["source"]}),
        json!({"inputs":["source"],"outputs":["missing"]}),
        json!({"inputs":["source"],"outputs":[]}),
    ] {
        let mut invalid = leaf_json();
        invalid["interface"] = interface;
        let candidate = definition(invalid);
        assert!(registry.create(candidate.clone(), None).is_err());
        assert!(registry
            .publish(&first.processor_id, candidate, &first.version, None)
            .is_err());
        assert_eq!(snapshot(&directory.0.join("registry")), before);
    }
}

#[test]
fn compiler_namespaces_private_relations_preserves_literals_and_broadcasts_inputs() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let mut leaf_definition = leaf_json();
    leaf_definition["rules"] =
        json!("middle(X) :- source(X). result(X) :- middle(X), X = \"source\".");
    let leaf = registry.create(definition(leaf_definition), None).unwrap();
    let mut broadcast = serde_json::to_value(manifest(&leaf)).unwrap();
    broadcast["inputs"]["input"]["targets"] = json!([
        {"node":"first","relation":"source"},
        {"node":"second","relation":"source"}
    ]);
    broadcast["bindings"] = json!([]);
    broadcast["outputs"] = json!({
        "left":{"node":"first","relation":"result"},
        "right":{"node":"second","relation":"result"}
    });
    let broadcast: CompositionManifest = serde_json::from_value(broadcast).unwrap();
    let compiled = registry.compile_composition(&broadcast).unwrap();
    let resolution = &compiled.resolution;
    assert_eq!(
        compiled
            .schemas
            .values()
            .filter(|schema| schema.input)
            .count(),
        1
    );
    assert_eq!(resolution.inputs.len(), 1);
    assert_eq!(resolution.outputs.len(), 2);
    assert_eq!(resolution.relations.len(), compiled.schemas.len());
    // String constants that happen to equal a local relation name must remain
    // data. Both copies of this leaf retain the exact literal.
    assert_eq!(compiled.source.matches("\"source\"").count(), 2);
    let mut private_physical = BTreeMap::new();
    for (physical, origin) in &resolution.relations {
        assert!(compiled.schemas.contains_key(physical));
        if origin["kind"] == "processor_relation" {
            let alias = origin["node"].as_str().unwrap();
            let selected = &broadcast.nodes[alias];
            assert_eq!(origin["processor_id"], selected.processor_id);
            assert_eq!(origin["version"], selected.version);
            assert_eq!(resolution.nodes[alias], *selected);
            assert_eq!(origin["schema"]["fields"], json!(["string"]));
            assert!(compiled.source.contains(&format!("R_{physical}(")));
            if origin["relation"] == "middle" {
                private_physical.insert(alias.to_string(), physical.clone());
            }
        }
    }
    assert_eq!(private_physical.len(), 2);
    assert_ne!(private_physical["first"], private_physical["second"]);

    let mut processor_rules = 0;
    let mut broadcast_targets = Vec::new();
    for origin in &resolution.rules {
        if origin["kind"] == "processor_rule" {
            processor_rules += 1;
            let alias = origin["node"].as_str().unwrap();
            let selected = &broadcast.nodes[alias];
            assert_eq!(origin["processor_id"], selected.processor_id);
            assert_eq!(origin["version"], selected.version);
            assert!(origin["rule"].as_u64().unwrap() < 2);
        }
        for direction in ["from", "to"] {
            if let Some(alias) = origin[direction]["node"].as_str() {
                assert_eq!(resolution.nodes[alias], broadcast.nodes[alias]);
                assert_eq!(resolution.nodes[alias], reference(&leaf));
            }
        }
        if origin["kind"] == "input_binding" {
            assert_eq!(origin["input"], "input");
            assert_eq!(origin["to"]["relation"], "source");
            broadcast_targets.push(origin["to"]["node"].as_str().unwrap());
        }
    }
    assert_eq!(processor_rules, 4);
    broadcast_targets.sort_unstable();
    assert_eq!(broadcast_targets, ["first", "second"]);
    let stored = registry.create(composed(broadcast), None).unwrap();
    assert_eq!(stored.composition.as_ref(), Some(resolution));
}

#[test]
fn archived_dependencies_preserve_existing_compositions_but_reject_new_references() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let leaf = registry.create(definition(leaf_json()), None).unwrap();
    let manifest = manifest(&leaf);
    let first = registry.create(composed(manifest.clone()), None).unwrap();
    let fork = registry
        .fork(&first.processor_id, &first.version, None)
        .unwrap();
    let compiled = registry.compile_composition(&manifest).unwrap();
    registry
        .archive(&leaf.processor_id, &leaf.version, 0)
        .unwrap();
    assert_eq!(
        registry
            .get(&leaf.processor_id, Some(&leaf.version))
            .unwrap(),
        leaf
    );
    assert_eq!(registry.get(&first.processor_id, None).unwrap(), first);
    assert_eq!(registry.get(&fork.processor_id, None).unwrap(), fork);
    assert_eq!(
        registry.compile_composition(&manifest).unwrap().source,
        compiled.source
    );
    assert_eq!(
        directory.registry().get(&first.processor_id, None).unwrap(),
        first
    );
    let before = snapshot(&directory.0.join("registry"));
    assert!(registry
        .create(composed(manifest.clone()), None)
        .unwrap_err()
        .contains("archived"));
    assert!(registry
        .publish(
            &first.processor_id,
            composed(manifest),
            &first.version,
            None
        )
        .unwrap_err()
        .contains("archived"));
    assert!(registry
        .fork(&first.processor_id, &first.version, None)
        .unwrap_err()
        .contains("archived"));
    assert_eq!(snapshot(&directory.0.join("registry")), before);
    let active = registry.list(10, None, false).unwrap();
    assert_eq!(active.processors.len(), 2);
    assert!(active
        .processors
        .iter()
        .all(|row| row.processor_id != leaf.processor_id));
    registry
        .archive(&first.processor_id, &first.version, 0)
        .unwrap();
    assert!(registry.ensure_active(&first.processor_id).is_err());
    assert_eq!(
        registry
            .get(&first.processor_id, Some(&first.version))
            .unwrap(),
        first
    );
    // A fork's lineage remains meaningful even after its source is archived.
    assert_eq!(registry.get(&fork.processor_id, None).unwrap(), fork);
    // Restoring eligibility changes no definitions or previously resolved graph.
    let restored = registry
        .restore(&leaf.processor_id, &leaf.version, 1)
        .unwrap();
    assert_eq!(restored.lifecycle_revision, 2);
    registry.ensure_active(&leaf.processor_id).unwrap();
    let new_composition = registry.create(first.definition.clone(), None).unwrap();
    assert_eq!(new_composition.version, first.version);
    assert_eq!(new_composition.composition, first.composition);
    registry
        .restore(&first.processor_id, &first.version, 1)
        .unwrap();
    assert_eq!(registry.get(&first.processor_id, None).unwrap(), first);
    assert!(registry
        .fork(&first.processor_id, &first.version, None)
        .is_ok());
}

// First-class nested-program acceptance is defined before recursive resolution.
#[test]
fn nested_program_interfaces_preserve_private_boundaries_and_exact_leaf_history() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let leaf = registry.create(definition(leaf_json()), None).unwrap();
    let inner = registry.create(composed(manifest(&leaf)), None).unwrap();
    let wrapper = wrapper_manifest(&inner);
    let outer = registry.create(composed(wrapper.clone()), None).unwrap();
    let compiled = registry.compile_composition(&wrapper).unwrap();
    assert_eq!(compiled.resolution.nodes, wrapper.nodes);
    assert_eq!(compiled.resolution.dependencies.len(), 3);
    assert_eq!(
        compiled
            .resolution
            .dependencies
            .values()
            .filter(|selected| **selected == reference(&leaf))
            .count(),
        2
    );
    assert_eq!(
        compiled
            .resolution
            .dependencies
            .values()
            .filter(|selected| **selected == reference(&inner))
            .count(),
        1
    );
    assert_eq!(
        compiled.resolution.inputs.keys().collect::<Vec<_>>(),
        vec!["wrapper_input"]
    );
    assert_eq!(
        compiled.resolution.outputs.keys().collect::<Vec<_>>(),
        vec!["wrapper_output"]
    );
    for origin in compiled
        .resolution
        .relations
        .values()
        .filter(|origin| origin["kind"] == "processor_relation")
    {
        assert_eq!(origin["processor_id"], leaf.processor_id);
        assert_eq!(origin["version"], leaf.version);
        let path = origin["node"].as_str().unwrap();
        assert!(path.starts_with("nested"));
        assert_eq!(compiled.resolution.dependencies[path], reference(&leaf));
    }
    let mut private = serde_json::to_value(&wrapper).unwrap();
    private["outputs"]["wrapper_output"]["relation"] = json!("result");
    assert!(registry
        .create(composed(serde_json::from_value(private).unwrap()), None)
        .is_err());
    let mut wrong_type = serde_json::to_value(&wrapper).unwrap();
    wrong_type["inputs"]["wrapper_input"]["fields"] = json!(["int"]);
    assert!(registry
        .create(composed(serde_json::from_value(wrong_type).unwrap()), None)
        .is_err());

    let mut next = leaf_json();
    next["rules"] = json!("middle(X) :- source(X). result(X) :- middle(X), X = \"source\".");
    let next = registry
        .publish(&leaf.processor_id, definition(next), &leaf.version, None)
        .unwrap();
    registry
        .archive(&leaf.processor_id, &next.version, 0)
        .unwrap();
    assert_eq!(registry.get(&outer.processor_id, None).unwrap(), outer);
    assert_eq!(
        registry.compile_composition(&wrapper).unwrap().source,
        compiled.source
    );
    assert!(registry
        .create(composed(wrapper.clone()), None)
        .unwrap_err()
        .contains("archived"));
    registry
        .restore(&leaf.processor_id, &next.version, 1)
        .unwrap();
    let second = registry.create(composed(wrapper), None).unwrap();
    assert_eq!(second.version, outer.version);
    assert_eq!(second.composition, outer.composition);
    assert!(registry
        .list(10, None, false)
        .unwrap()
        .processors
        .iter()
        .all(|row| serde_json::to_value(row).unwrap()["kind"] == "program"));
}

#[test]
fn cyclic_node_wiring_is_valid_when_expanded_rule_dependencies_are_acyclic() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let leaf = registry
        .create(
            definition(json!({
                "rules":"result(X) :- seed(X).",
                "schemas":{
                    "seed":{"input":true,"fields":["string"]},
                    "unused":{"input":true,"fields":["string"]},
                    "result":{"input":false,"fields":["string"]}
                },
                "interface":{"inputs":["seed","unused"],"outputs":["result"]}
            })),
            None,
        )
        .unwrap();
    let manifest: CompositionManifest = serde_json::from_value(json!({
        "nodes":{"left":reference(&leaf),"right":reference(&leaf)},
        "inputs":{"input":{"fields":["string"],"targets":[{"node":"left","relation":"seed"},{"node":"right","relation":"seed"}]}},
        "bindings":[
            {"from":{"node":"left","relation":"result"},"to":{"node":"right","relation":"unused"}},
            {"from":{"node":"right","relation":"result"},"to":{"node":"left","relation":"unused"}}
        ],
        "outputs":{"output":{"node":"left","relation":"result"}}
    })).unwrap();
    assert!(registry.create(composed(manifest), None).is_ok());
}

#[test]
fn excessive_exact_reference_nesting_fails_with_a_bounded_actionable_error() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let leaf = registry.create(definition(leaf_json()), None).unwrap();
    let inner = registry.create(composed(manifest(&leaf)), None).unwrap();
    let template = serde_json::to_value(&inner).unwrap();
    let mut previous = reference(&inner);
    // Simulate imported/corrupt but content-address-valid records. Resolution
    // must stop at its traversal bound before compiling or trusting metadata.
    for index in 1..=130 {
        let authored = json!({"composition":{
            "nodes":{"nested":previous},
            "inputs":{"input":{"fields":["string"],"targets":[{"node":"nested","relation":"input"}]}},
            "bindings":[],
            "outputs":{"output":{"node":"nested","relation":"output"}}
        }});
        let sha = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&authored).unwrap())
        );
        let id = format!("processor_{index:032x}");
        let version = format!("sha256:{sha}");
        let mut record = template.clone();
        record["processor_id"] = json!(id);
        record["version"] = json!(version);
        record["content_sha256"] = json!(sha);
        record["definition"] = authored;
        let versions = directory.0.join("registry").join(&id).join("versions");
        fs::create_dir_all(&versions).unwrap();
        fs::write(
            versions.join(format!("{sha}.json")),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        previous = ProcessorReference {
            processor_id: id,
            version,
        };
    }
    let error = registry
        .get(&previous.processor_id, Some(&previous.version))
        .unwrap_err();
    assert!(
        error.contains("128") && error.contains("simplify"),
        "{error}"
    );
}

#[test]
fn repeated_program_expansion_has_a_bounded_actionable_resource_limit() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let leaf = registry.create(definition(leaf_json()), None).unwrap();
    let mut authored = serde_json::to_value(manifest(&leaf)).unwrap();
    let nodes: serde_json::Map<String, Value> = (0..4097)
        .map(|index| (format!("node{index}"), json!(reference(&leaf))))
        .collect();
    authored["nodes"] = Value::Object(nodes);
    let before = snapshot(&directory.0.join("registry"));
    let error = registry
        .create(composed(serde_json::from_value(authored).unwrap()), None)
        .unwrap_err();
    assert!(
        error.contains("4096") && error.contains("simplify"),
        "{error}"
    );
    assert_eq!(snapshot(&directory.0.join("registry")), before);
}
