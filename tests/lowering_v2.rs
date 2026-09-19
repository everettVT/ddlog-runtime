#![cfg(unix)]
//! Lowering version 2: lean composition text with identical public semantics.
//! Pure tests compare generated text and registry records; the fake driver
//! checks the instance contract; the `#[ignore]` native tests prove relation
//! contents match under both lowerings and measure the operator count.
use ddlog_runtime::composition::CompositionManifest;
use ddlog_runtime::registry::{
    CompositionDefinition, ProcessorDefinition, ProcessorReference, ProcessorRegistry,
    ProcessorVersion,
};
use ddlog_runtime::{lower_with_options, Backend, LoweringOptions, ProgramInstance, Schema};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct TestDirectory(PathBuf);
impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "lowering-v2-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn registry(&self) -> ProcessorRegistry {
        ProcessorRegistry::open(self.0.join("registry")).unwrap()
    }
    /// The simulated transport of `tests/worlds.rs`: not a Datalog evaluator.
    fn fake_driver(&self) -> PathBuf {
        let template =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/memory_fake_runtime.py");
        let driver = self.0.join("build.py");
        fs::write(&driver, format!("#!/usr/bin/env python3\nimport sys\nfrom pathlib import Path\nsource=Path({}).read_text().replace('__CONTROL__',{})\nPath(sys.argv[2]).write_text(source)\nPath(sys.argv[2]).chmod(0o700)\n", json!(template), json!(serde_json::to_string(&self.0).unwrap()))).unwrap();
        fs::set_permissions(&driver, fs::Permissions::from_mode(0o700)).unwrap();
        driver
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
fn reference(record: &ProcessorVersion) -> ProcessorReference {
    ProcessorReference {
        processor_id: record.processor_id.clone(),
        version: record.version.clone(),
    }
}
fn composed(manifest: &CompositionManifest) -> ProcessorDefinition {
    ProcessorDefinition::Composition(CompositionDefinition {
        inspection: None,
        composition: manifest.clone(),
    })
}
/// `filter` keeps items above a threshold and reports the rest; `pair` joins
/// its two inputs on the number, with a comparison and a negation. Together
/// they exercise a broadcast input, a node-to-node binding, negation over a
/// bound input and every kind of external port.
fn filter_json() -> Value {
    json!({
        "rules":"kept(X,N) :- item(X,N), N > 1. dropped(X) :- item(X,N), !kept(X,N).",
        "schemas":{
            "item":{"input":true,"fields":["string","int"]},
            "kept":{"input":false,"fields":["string","int"]},
            "dropped":{"input":false,"fields":["string"]}
        },
        "interface":{"inputs":["item"],"outputs":["kept","dropped"]}
    })
}
fn pair_json() -> Value {
    json!({
        "rules":"pair(X,Y) :- left(X,N), right(Y,N), X \\= Y. lonely(X) :- left(X,N), !right(X,N).",
        "schemas":{
            "left":{"input":true,"fields":["string","int"]},
            "right":{"input":true,"fields":["string","int"]},
            "pair":{"input":false,"fields":["string","string"]},
            "lonely":{"input":false,"fields":["string"]}
        },
        "interface":{"inputs":["left","right"],"outputs":["pair","lonely"]}
    })
}
fn chain_manifest(filter: &ProcessorVersion, pair: &ProcessorVersion) -> CompositionManifest {
    serde_json::from_value(json!({
        "nodes":{"filter":reference(filter),"pair":reference(pair)},
        "inputs":{"items":{"fields":["string","int"],"targets":[{"node":"filter","relation":"item"},{"node":"pair","relation":"right"}]}},
        "bindings":[{"from":{"node":"filter","relation":"kept"},"to":{"node":"pair","relation":"left"}}],
        "outputs":{"pairs":{"node":"pair","relation":"pair"},"lonely":{"node":"pair","relation":"lonely"},"dropped":{"node":"filter","relation":"dropped"}}
    }))
    .unwrap()
}
fn wrapper_manifest(chain: &ProcessorVersion) -> CompositionManifest {
    serde_json::from_value(json!({
        "nodes":{"inner":reference(chain)},
        "inputs":{"rows":{"fields":["string","int"],"targets":[{"node":"inner","relation":"items"}]}},
        "bindings":[],
        "outputs":{"pairs":{"node":"inner","relation":"pairs"},"dropped":{"node":"inner","relation":"dropped"}}
    }))
    .unwrap()
}
/// Leaves, the chain and its wrapper, all registered under lowering version 1.
fn registered(registry: &ProcessorRegistry) -> (ProcessorVersion, ProcessorVersion) {
    let filter = registry.create(definition(filter_json()), None).unwrap();
    let pair = registry.create(definition(pair_json()), None).unwrap();
    let chain = registry
        .create(composed(&chain_manifest(&filter, &pair)), None)
        .unwrap();
    let wrapper = registry
        .create(composed(&wrapper_manifest(&chain)), None)
        .unwrap();
    (chain, wrapper)
}
fn declarations<'a>(source: &'a str, keyword: &str) -> BTreeSet<&'a str> {
    source
        .lines()
        .filter_map(|line| line.strip_prefix(keyword))
        .map(|rest| rest.split('(').next().unwrap())
        .collect()
}
fn sha(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[test]
fn version_2_drops_evidence_exports_only_public_outputs_and_aliases_bindings() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let (chain, _) = registered(&registry);
    let manifest = match &chain.definition {
        ProcessorDefinition::Composition(definition) => definition.composition.clone(),
        _ => unreachable!(),
    };
    let v1 = registry.compile_composition(&manifest).unwrap();
    let v2 = registry
        .compile_composition_versioned(&manifest, 2)
        .unwrap();
    assert_eq!(
        v1.resolution,
        *chain.composition.as_ref().unwrap(),
        "version 1 is the registered form"
    );
    assert!(v1.source.contains("output relation Evidence0("));
    assert!(!v2.source.contains("Evidence"), "{}", v2.source);
    assert_eq!(v2.resolution.lowering_version, 2);
    assert_eq!(v2.resolution.generated_source_sha256, sha(&v2.source));
    assert_ne!(
        v1.resolution.generated_source_sha256,
        v2.resolution.generated_source_sha256
    );
    // Public naming is unchanged: the same input and output relation names.
    assert_eq!(v1.resolution.inputs, v2.resolution.inputs);
    assert_eq!(v1.resolution.outputs, v2.resolution.outputs);
    assert_eq!(v1.resolution.nodes, v2.resolution.nodes);
    assert_eq!(v1.resolution.dependencies, v2.resolution.dependencies);
    // Only external outputs and the relations they copy are exported.
    assert_eq!(
        declarations(&v2.source, "output relation R_"),
        BTreeSet::from([
            "Output_pairs",
            "Output_lonely",
            "Output_dropped",
            "Module1_pair",
            "Module1_lonely",
            "Module0_dropped"
        ])
    );
    assert_eq!(
        declarations(&v2.source, "relation R_"),
        BTreeSet::from(["Module0_kept"])
    );
    assert_eq!(
        declarations(&v2.source, "input relation R_"),
        BTreeSet::from(["Input_items"])
    );
    // Bound inputs are aliased away: no relation, no copy rule, direct reads.
    for aliased in ["Module0_item", "Module1_left", "Module1_right"] {
        assert!(!v2.source.contains(&format!("R_{aliased}")), "{aliased}");
        assert!(v1.source.contains(&format!("R_{aliased}(")), "{aliased}");
    }
    assert!(v2
        .source
        .contains("R_Module0_kept(v_X, v_N) :- R_Input_items(v_X, v_N), v_N > 1.\n"));
    assert!(v2.source.contains(
        "R_Module1_pair(v_X, v_Y) :- R_Module0_kept(v_X, v_N), R_Input_items(v_Y, v_N), v_X != v_Y.\n"
    ));
    assert!(v2.source.contains(
        "R_Module1_lonely(v_X) :- R_Module0_kept(v_X, v_N), not R_Input_items(v_X, v_N).\n"
    ));
    assert!(v2
        .source
        .contains("R_Output_pairs(v_X0, v_X1) :- R_Module1_pair(v_X0, v_X1).\n"));
    let alias = |name: &str| v2.resolution.relations[name]["alias_of"].clone();
    assert_eq!(alias("Module0_item"), json!("Input_items"));
    assert_eq!(alias("Module1_right"), json!("Input_items"));
    assert_eq!(alias("Module1_left"), json!("Module0_kept"));
    assert!(v1.resolution.relations["Module0_item"]
        .get("alias_of")
        .is_none());
    assert_eq!(v1.resolution.relations.len(), v2.resolution.relations.len());
    // Aliased bindings generate no rule; the remaining origins keep their order.
    let kinds = |resolution: &ddlog_runtime::composition::CompositionResolution| {
        resolution
            .rules
            .iter()
            .map(|origin| origin["kind"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        kinds(&v1.resolution),
        [
            "processor_rule",
            "processor_rule",
            "processor_rule",
            "processor_rule",
            "input_binding",
            "input_binding",
            "processor_binding",
            "output_binding",
            "output_binding",
            "output_binding"
        ]
    );
    assert_eq!(
        kinds(&v2.resolution),
        [
            "processor_rule",
            "processor_rule",
            "processor_rule",
            "processor_rule",
            "output_binding",
            "output_binding",
            "output_binding"
        ]
    );
    assert_eq!(v2.source.matches(" :- ").count(), v2.resolution.rules.len());
    assert_eq!(
        v1.source.matches(" :- ").count(),
        2 * v1.resolution.rules.len()
    );
}

#[test]
fn nested_aliases_resolve_through_composite_inputs() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let (chain, wrapper) = registered(&registry);
    let manifest = wrapper_manifest(&chain);
    let v2 = registry
        .compile_composition_versioned(&manifest, 2)
        .unwrap();
    assert!(!v2.source.contains("Evidence"));
    assert!(!v2.source.contains("R_Composite0_Input_items"));
    assert!(v2
        .source
        .contains("R_Module1_kept(v_X, v_N) :- R_Input_rows(v_X, v_N), v_N > 1.\n"));
    let alias = |name: &str| v2.resolution.relations[name]["alias_of"].clone();
    assert_eq!(alias("Composite0_Input_items"), json!("Input_rows"));
    assert_eq!(alias("Module1_item"), json!("Input_rows"));
    assert_eq!(alias("Module2_right"), json!("Input_rows"));
    assert_eq!(alias("Module2_left"), json!("Module1_kept"));
    // The nested composite's own outputs are internal copies, exported only
    // when a top-level output reads them directly.
    assert_eq!(
        declarations(&v2.source, "output relation R_"),
        BTreeSet::from([
            "Output_pairs",
            "Output_dropped",
            "Composite0_Output_pairs",
            "Composite0_Output_dropped"
        ])
    );
    assert_eq!(
        declarations(&v2.source, "relation R_"),
        BTreeSet::from([
            "Module1_kept",
            "Module2_pair",
            "Module2_lonely",
            "Module1_dropped",
            "Composite0_Output_lonely"
        ])
    );
    assert_eq!(
        v2.resolution.inputs,
        wrapper.composition.as_ref().unwrap().inputs
    );
    assert_eq!(
        v2.resolution.outputs,
        wrapper.composition.as_ref().unwrap().outputs
    );
}

#[test]
fn ordinary_programs_lower_under_explicit_options() {
    let schemas: BTreeMap<String, Schema> =
        serde_json::from_value(filter_json()["schemas"].clone()).unwrap();
    let rules = filter_json()["rules"].as_str().unwrap().to_string();
    let v1 = lower_with_options(&rules, &schemas, &[], &LoweringOptions::VERSION_1, None).unwrap();
    assert_eq!(v1, ddlog_runtime::lower(&rules, &schemas).unwrap());
    let exports = BTreeSet::from(["dropped".to_string()]);
    let v2 = lower_with_options(
        &rules,
        &schemas,
        &[],
        &LoweringOptions::VERSION_2,
        Some(&exports),
    )
    .unwrap();
    assert_eq!(
        v2,
        "output relation R_dropped(f0: string)\ninput relation R_item(f0: string, f1: signed<64>)\nrelation R_kept(f0: string, f1: signed<64>)\nR_kept(v_X, v_N) :- R_item(v_X, v_N), v_N > 1.\nR_dropped(v_X) :- R_item(v_X, v_N), not R_kept(v_X, v_N).\n"
    );
    // Without an export set every derived relation stays public.
    let open =
        lower_with_options(&rules, &schemas, &[], &LoweringOptions::VERSION_2, None).unwrap();
    assert!(open.contains("output relation R_kept("));
    assert_eq!(LoweringOptions::for_version(1).unwrap().version(), Some(1));
    assert_eq!(LoweringOptions::for_version(2).unwrap().version(), Some(2));
    assert!(LoweringOptions::for_version(3)
        .unwrap_err()
        .contains("versions 1 and 2"));
    assert_eq!(
        LoweringOptions {
            explain: true,
            export_internal: false,
            alias_bindings: false
        }
        .version(),
        None
    );
    assert_eq!(LoweringOptions::default(), LoweringOptions::VERSION_1);
}

#[test]
fn registered_version_1_compositions_still_read_and_version_2_records_round_trip() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let (chain, wrapper) = registered(&registry);
    let manifest = chain_manifest_of(&chain);
    // Version 1 records carry no lowering_version field and keep verifying.
    let serialized = serde_json::to_value(&chain).unwrap();
    assert!(serialized["composition"].get("lowering_version").is_none());
    assert_eq!(
        registry
            .get(&chain.processor_id, Some(&chain.version))
            .unwrap(),
        chain
    );
    assert_eq!(registry.get(&wrapper.processor_id, None).unwrap(), wrapper);
    // A version 2 registration of the same manifest is a new processor
    // whose resolution records version 2 and the lean hash.
    let lean = registry
        .create_versioned(composed(&manifest), None, 2)
        .unwrap();
    assert_eq!(
        lean.version, chain.version,
        "identity is the authored definition"
    );
    assert_ne!(lean.processor_id, chain.processor_id);
    let resolution = lean.composition.as_ref().unwrap();
    assert_eq!(resolution.lowering_version, 2);
    assert_eq!(
        resolution.generated_source_sha256,
        registry
            .compile_composition_versioned(&manifest, 2)
            .unwrap()
            .resolution
            .generated_source_sha256
    );
    assert_eq!(
        serde_json::to_value(&lean).unwrap()["composition"]["lowering_version"],
        json!(2)
    );
    assert_eq!(
        registry
            .get(&lean.processor_id, Some(&lean.version))
            .unwrap(),
        lean
    );
    assert_eq!(registry.get(&lean.processor_id, None).unwrap(), lean);
    assert_eq!(
        registry.versions(&lean.processor_id).unwrap(),
        vec![lean.clone()]
    );
    // A composition over a version 2 node verifies that node under its own
    // recorded version, whichever version the parent records.
    let parent = registry
        .create(composed(&wrapper_manifest(&lean)), None)
        .unwrap();
    assert_eq!(registry.get(&parent.processor_id, None).unwrap(), parent);
    assert_eq!(parent.composition.as_ref().unwrap().lowering_version, 1);
    let lean_parent = registry
        .create_versioned(composed(&wrapper_manifest(&lean)), None, 2)
        .unwrap();
    assert_eq!(
        lean_parent.composition.as_ref().unwrap().lowering_version,
        2
    );
    assert_eq!(
        registry.get(&lean_parent.processor_id, None).unwrap(),
        lean_parent
    );
    // Imports verify the record under its recorded version too, dependency
    // closure first.
    let other = TestDirectory::new();
    let target = other.registry();
    for dependency in chain.composition.as_ref().unwrap().dependencies.values() {
        let leaf = registry
            .get(&dependency.processor_id, Some(&dependency.version))
            .unwrap();
        target.import_version(leaf).unwrap();
    }
    for record in [&chain, &lean] {
        target.import_version(record.clone()).unwrap();
        assert_eq!(
            target
                .get(&record.processor_id, Some(&record.version))
                .unwrap(),
            *record
        );
    }
    // A tampered lowering_version no longer matches the recorded hash.
    let path = directory
        .0
        .join("registry")
        .join(&lean.processor_id)
        .join("versions")
        .join(format!("{}.json", lean.content_sha256));
    let mut tampered: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    tampered["composition"]["lowering_version"] = json!(1);
    fs::write(&path, serde_json::to_vec(&tampered).unwrap()).unwrap();
    assert!(registry
        .get(&lean.processor_id, Some(&lean.version))
        .unwrap_err()
        .contains("resolution mismatch"));
    // Wrong version numbers are rejected before anything is written.
    assert!(registry
        .create_versioned(composed(&manifest), None, 3)
        .unwrap_err()
        .contains("versions 1 and 2"));
}
fn chain_manifest_of(chain: &ProcessorVersion) -> CompositionManifest {
    match &chain.definition {
        ProcessorDefinition::Composition(definition) => definition.composition.clone(),
        _ => unreachable!(),
    }
}

#[test]
fn instances_build_version_2_text_without_changing_the_pinned_record() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let (chain, _) = registered(&registry);
    let driver = directory.fake_driver();
    let owner = |name: &str| {
        ProgramInstance::new(
            Backend::new(directory.0.join(name), driver.clone()),
            BTreeMap::new(),
            Some(directory.registry()),
            Some(name.into()),
        )
    };
    let pin = json!({"processor_id":chain.processor_id,"version":chain.version});
    let mut classic = owner("classic");
    let mut lean = owner("lean");
    lean.set_lowering_version(2).unwrap();
    assert!(lean
        .set_lowering_version(7)
        .unwrap_err()
        .contains("versions 1 and 2"));
    let installed_classic = classic.execute("processor_install", &pin).unwrap();
    let installed_lean = lean.execute("processor_install", &pin).unwrap();
    assert_eq!(installed_classic["lowering_version"], 1);
    assert_eq!(installed_lean["lowering_version"], 2);
    assert_eq!(installed_lean["processor"], pin);
    assert_eq!(installed_lean["composition"]["lowering_version"], 2);
    assert!(installed_classic["composition"]
        .get("lowering_version")
        .is_none());
    let manifest = chain_manifest_of(&chain);
    let expected_v2 = registry
        .compile_composition_versioned(&manifest, 2)
        .unwrap();
    for (instance, version, expected_sha) in [
        (
            &mut classic,
            1,
            chain
                .composition
                .as_ref()
                .unwrap()
                .generated_source_sha256
                .clone(),
        ),
        (
            &mut lean,
            2,
            expected_v2.resolution.generated_source_sha256.clone(),
        ),
    ] {
        let info = instance.execute("instance_info", &json!({})).unwrap();
        assert_eq!(info["lowering_version"], version);
        assert_eq!(info["source_sha256"], json!(expected_sha));
        let source = instance.execute("program_source", &json!({})).unwrap();
        assert_eq!(source["source_sha256"], json!(expected_sha));
        assert_eq!(sha(source["source"].as_str().unwrap()), expected_sha);
        assert_eq!(
            source["source"].as_str().unwrap().contains("Evidence"),
            version == 1
        );
    }
    // The registered record is untouched by either build.
    assert_eq!(
        registry
            .get(&chain.processor_id, Some(&chain.version))
            .unwrap(),
        chain
    );
    // The same public contract answers identically through both instances.
    let changes = json!({"changes":[
        {"op":"insert","predicate":"items","values":["a",1]},
        {"op":"insert","predicate":"items","values":["b",2]}
    ]});
    let mut answers = Vec::new();
    for instance in [&mut classic, &mut lean] {
        let applied = instance.execute("apply_changes", &changes).unwrap();
        let mut answer = vec![json!({"revision":applied["revision"],"deltas":applied["deltas"]})];
        answer.push(instance.execute("relations", &json!({})).unwrap());
        for predicate in ["items", "pairs", "lonely", "dropped"] {
            answer.push(
                instance
                    .execute("query_rows", &json!({"predicate":predicate}))
                    .unwrap(),
            );
        }
        answer.push(
            instance
                .execute("lemmalog_query", &json!({"predicate":"pairs"}))
                .unwrap(),
        );
        answers.push(answer);
    }
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[0][2]["total"], 2);
    // Witnesses exist under version 1 only; version 2 says so explicitly.
    let why = classic.execute("lemmalog_why", &json!({"rule":0})).unwrap();
    assert_eq!(why["origin"]["kind"], "processor_rule");
    let error = lean
        .execute("lemmalog_why", &json!({"rule":0}))
        .unwrap_err();
    assert!(
        error.contains("Explanations were not compiled") && error.contains("version 2"),
        "{error}"
    );
    // An install argument selects the version per build without touching the
    // instance default; `processor_create` records the requested version.
    let mut explicit = owner("explicit");
    let installed = explicit
        .execute(
            "processor_install",
            &json!({"processor_id":chain.processor_id,"version":chain.version,"lowering_version":2}),
        )
        .unwrap();
    assert_eq!(installed["lowering_version"], 2);
    assert!(explicit
        .execute(
            "processor_install",
            &json!({"processor_id":chain.processor_id,"lowering_version":9})
        )
        .unwrap_err()
        .contains("pinned"));
    let mut author = owner("author");
    assert!(author
        .execute(
            "processor_create",
            &json!({"definition":{"composition":manifest},"lowering_version":"2"}),
        )
        .unwrap_err()
        .contains("lowering_version must be 1 or 2"));
    let created = author
        .execute(
            "processor_create",
            &json!({"definition":{"composition":manifest},"lowering_version":2}),
        )
        .unwrap();
    assert_eq!(created["composition"]["lowering_version"], 2);
    let fetched = author
        .execute(
            "processor_get",
            &json!({"processor_id":created["processor_id"],"version":created["version"]}),
        )
        .unwrap();
    assert_eq!(fetched, created);
    // A program pinned with an interface exports only its interface outputs
    // under version 2 while its public relations stay addressable.
    let filter = registry.create(definition(filter_json()), None).unwrap();
    let mut program = owner("program");
    program.set_lowering_version(2).unwrap();
    program
        .execute(
            "processor_install",
            &json!({"processor_id":filter.processor_id,"version":filter.version}),
        )
        .unwrap();
    let source = program.execute("program_source", &json!({})).unwrap();
    let text = source["source"].as_str().unwrap();
    assert!(
        text.contains("output relation R_dropped(") && text.contains("output relation R_kept(")
    );
    assert!(!text.contains("Evidence"));
    assert_eq!(
        program.execute("relations", &json!({})).unwrap()["relations"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn checkpoints_record_the_lowering_version_of_their_text() {
    let directory = TestDirectory::new();
    let driver = directory.fake_driver();
    let rules = filter_json()["rules"].as_str().unwrap().to_string();
    let schemas = filter_json()["schemas"].clone();
    let mut lean = Backend::new(directory.0.join("lean"), driver.clone());
    lean.set_lowering_version(2).unwrap();
    assert_eq!(lean.lowering_version(), 2);
    let installed = lean.install(&rules, schemas.clone()).unwrap();
    assert_eq!(installed["lowering_version"], 2);
    assert!(!lean.program_source().contains("Evidence"));
    assert_eq!(lean.lowering(), LoweringOptions::VERSION_2);
    lean.apply(&json!([{"op":"insert","predicate":"item","values":["a",1]}]))
        .unwrap();
    let bytes = lean.checkpoint_bytes(json!({"note":"lean"})).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("\"lowering_version\":2"));
    let mut restored = Backend::new(directory.0.join("restored"), driver.clone());
    assert_eq!(
        restored.restore_checkpoint_bytes(&bytes).unwrap(),
        json!({"note":"lean"})
    );
    assert_eq!(restored.lowering(), LoweringOptions::VERSION_2);
    assert_eq!(restored.source_sha256(), lean.source_sha256());
    assert!(restored
        .why(0)
        .unwrap_err()
        .contains("Explanations were not compiled"));
    // Version 1 checkpoints keep their pre-existing byte layout: no field.
    let mut classic = Backend::new(directory.0.join("classic"), driver);
    classic.install(&rules, schemas).unwrap();
    let bytes = classic.checkpoint_bytes(json!({})).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("lowering_version"));
    assert!(classic.why(0).is_ok());
}

/// The shared native harness: an owner-configured driver, one instance per
/// lowering version, and typed rows of every public relation after each step.
fn native_driver() -> Option<PathBuf> {
    std::env::var_os("DDLOG_RUNTIME_NATIVE_BUILD").map(PathBuf::from)
}
fn public_rows(instance: &mut ProgramInstance) -> BTreeMap<String, Vec<Value>> {
    let relations = instance.execute("relations", &json!({})).unwrap();
    let mut rows = BTreeMap::new();
    for relation in relations["relations"].as_array().unwrap() {
        let name = relation["name"].as_str().unwrap();
        let page = instance
            .execute("query_rows", &json!({"predicate":name,"max_rows":1000}))
            .unwrap();
        assert_eq!(page["complete"], true);
        let mut values = page["rows"].as_array().unwrap().clone();
        values.sort_by_key(|row| row.to_string());
        assert_eq!(page["total"], values.len());
        assert_eq!(relation["count"], values.len());
        rows.insert(name.to_string(), values);
    }
    rows
}
fn sorted(rows: Value) -> Vec<Value> {
    let mut rows = rows.as_array().unwrap().clone();
    rows.sort_by_key(|row| row.to_string());
    rows
}

#[test]
#[ignore = "requires DDLOG_RUNTIME_NATIVE_BUILD and its operator-configured native toolchain"]
fn native_public_relations_are_identical_under_both_lowerings() {
    let driver = native_driver().expect("Configure native build driver");
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let (_, wrapper) = registered(&registry);
    let pin = json!({"processor_id":wrapper.processor_id,"version":wrapper.version});
    let mut instances = Vec::new();
    for version in [1_u32, 2] {
        let mut instance = ProgramInstance::new(
            Backend::new(directory.0.join(format!("v{version}")), driver.clone()),
            BTreeMap::new(),
            Some(directory.registry()),
            Some(format!("v{version}")),
        );
        instance.set_lowering_version(version).unwrap();
        let installed = instance.execute("processor_install", &pin).unwrap();
        assert_eq!(installed["lowering_version"], version);
        instances.push(instance);
    }
    let steps = [
        json!([{"op":"insert","predicate":"rows","values":["a",1]},{"op":"insert","predicate":"rows","values":["b",2]},
               {"op":"insert","predicate":"rows","values":["c",2]},{"op":"insert","predicate":"rows","values":["d",3]}]),
        json!([{"op":"delete","predicate":"rows","values":["c",2]}]),
        json!([{"op":"insert","predicate":"rows","values":["e",2]},{"op":"delete","predicate":"rows","values":["a",1]}]),
        json!([{"op":"delete","predicate":"rows","values":["b",2]},{"op":"delete","predicate":"rows","values":["d",3]},
               {"op":"delete","predicate":"rows","values":["e",2]}]),
    ];
    let expected_pairs = [
        json!([["b", "c"], ["c", "b"]]),
        json!([]),
        json!([["b", "e"], ["e", "b"]]),
        json!([]),
    ];
    let expected_dropped = [json!([["a"]]), json!([["a"]]), json!([]), json!([])];
    let mut observed = [
        public_rows(&mut instances[0]),
        public_rows(&mut instances[1]),
    ];
    assert_eq!(observed[0], observed[1]);
    for (step, changes) in steps.iter().enumerate() {
        for (index, instance) in instances.iter_mut().enumerate() {
            let applied = instance
                .execute("apply_changes", &json!({"changes":changes}))
                .unwrap();
            assert_eq!(applied["revision"], step as u64 + 2);
            observed[index] = public_rows(instance);
        }
        assert_eq!(observed[0], observed[1], "step {step}");
        assert_eq!(observed[0]["pairs"], sorted(expected_pairs[step].clone()));
        assert_eq!(
            observed[0]["dropped"],
            sorted(expected_dropped[step].clone())
        );
    }
    let why = instances[0]
        .execute("lemmalog_why", &json!({"rule":0}))
        .unwrap();
    assert_eq!(why["origin"]["kind"], "processor_rule");
    assert!(instances[1]
        .execute("lemmalog_why", &json!({"rule":0}))
        .unwrap_err()
        .contains("Explanations were not compiled"));
}

/// Measure a registered composition: relation and rule counts of both
/// lowerings and, with a native driver and `DDLOG_OBSERVER_FILE` exported for
/// the child, the native operator count (`Operates` events) of the selected
/// version. Configure `DDLOG_LOWERING_REGISTRY` (a registry directory, read
/// through a private copy), `DDLOG_LOWERING_PROCESSOR`, optionally
/// `DDLOG_LOWERING_VERSION` (default 2) and `DDLOG_LOWERING_OUT` for the text.
#[test]
#[ignore = "measurement over an operator-supplied registry; run with --nocapture"]
fn measure_registered_composition_lowerings() {
    let source = PathBuf::from(std::env::var_os("DDLOG_LOWERING_REGISTRY").expect("registry"));
    let processor_id = std::env::var("DDLOG_LOWERING_PROCESSOR").expect("processor");
    let version: u32 = std::env::var("DDLOG_LOWERING_VERSION")
        .ok()
        .map_or(2, |v| v.parse().unwrap());
    let directory = TestDirectory::new();
    private_copy(&source, &directory.0.join("registry"));
    let registry = directory.registry();
    let record = registry.get(&processor_id, None).unwrap();
    let manifest = chain_manifest_of(&record);
    let recorded = record.composition.as_ref().unwrap();
    let mut report = json!({"processor_id":processor_id,"version":record.version,
        "recorded_lowering_version":recorded.lowering_version,"lowerings":{}});
    for lowering in [1_u32, 2] {
        let compiled = registry
            .compile_composition_versioned(&manifest, lowering)
            .unwrap();
        if lowering == recorded.lowering_version {
            assert_eq!(compiled.resolution, *recorded);
        }
        let count = |keyword: &str| {
            compiled
                .source
                .lines()
                .filter(|l| l.starts_with(keyword))
                .count()
        };
        report["lowerings"][lowering.to_string()] = json!({
            "sha256":compiled.resolution.generated_source_sha256,
            "input_relations":count("input relation R_"),
            "output_relations":count("output relation R_"),
            "internal_relations":count("relation R_"),
            "evidence_relations":count("output relation Evidence"),
            "rules":compiled.source.matches(" :- ").count(),
            "aliased_bindings":compiled.resolution.relations.values().filter(|r| r.get("alias_of").is_some()).count(),
            "bytes":compiled.source.len(),
        });
        if let Some(out) = std::env::var_os("DDLOG_LOWERING_OUT") {
            fs::write(
                Path::new(&out).join(format!("program-v{lowering}.dl")),
                &compiled.source,
            )
            .unwrap();
        }
    }
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    let Some(driver) = native_driver() else {
        println!("native build skipped: DDLOG_RUNTIME_NATIVE_BUILD is not set");
        return;
    };
    let mut instance = ProgramInstance::new(
        Backend::new(directory.0.join("native"), driver),
        BTreeMap::new(),
        Some(directory.registry()),
        Some("measure".into()),
    );
    instance.set_lowering_version(version).unwrap();
    let started = std::time::Instant::now();
    let installed = instance
        .execute(
            "processor_install",
            &json!({"processor_id":record.processor_id,"version":record.version}),
        )
        .unwrap();
    let info = instance.execute("instance_info", &json!({})).unwrap();
    println!(
        "native build of lowering version {} in {:?}: install {} info {}",
        version,
        started.elapsed(),
        installed,
        json!({"source_sha256":info["source_sha256"],"lowering_version":info["lowering_version"]})
    );
    assert_eq!(installed["lowering_version"], version);
    let relations = instance.execute("relations", &json!({})).unwrap();
    println!(
        "public relations: {}",
        relations["relations"].as_array().unwrap().len()
    );
    if let Some(capture) = std::env::var_os("DDLOG_OBSERVER_FILE") {
        // Topology events are written at startup; give the child a moment.
        std::thread::sleep(std::time::Duration::from_secs(3));
        let text = fs::read_to_string(&capture).unwrap();
        let mut names: BTreeMap<String, usize> = BTreeMap::new();
        let mut operates = 0;
        for line in text.lines() {
            let event: Value = serde_json::from_str(line).unwrap();
            if let Some(node) = event["event"].get("Operates") {
                operates += 1;
                *names
                    .entry(node["name"].as_str().unwrap().to_string())
                    .or_default() += 1;
            }
        }
        println!("Operates events (lowering version {version}): {operates}");
        let mut ranked: Vec<_> = names.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (name, count) in ranked.iter().take(24) {
            println!("{count:6} {name}");
        }
    }
}
fn private_copy(source: &Path, destination: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(destination)
        .unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            private_copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
            fs::set_permissions(target, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}
