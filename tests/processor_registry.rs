#![cfg(unix)]

use ddlog_runtime::registry::{
    GitProvenance, ProcessorDefinition, ProcessorReference, ProcessorRegistry, ProcessorStatus,
    ProgramDefinition, RegisteredOperationBinding,
};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
struct TestDirectory(PathBuf);
impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "lemmalog-registry-test-{}-{}",
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
fn definition() -> ProcessorDefinition {
    ProcessorDefinition::Program(ProgramDefinition {
        rules: "visible(X) :- item(X).".into(),
        schemas: json!({
            "item": {"input":true,"fields":["string"]},
            "visible": {"input":false,"fields":["string"]}
        }),
        operation: None,
        interface: None,
        operators: Vec::new(),
    })
}
fn program(definition: &mut ProcessorDefinition) -> &mut ProgramDefinition {
    match definition {
        ProcessorDefinition::Program(program) => program,
        ProcessorDefinition::Composition(_) => panic!("Expected a program fixture"),
    }
}
fn provenance(revision: &str) -> Option<GitProvenance> {
    Some(GitProvenance {
        repository: "https://example.invalid/processors.git".into(),
        revision: revision.into(),
        path: Some("processors/example.json".into()),
    })
}

#[test]
fn identity_content_and_provenance_are_distinct_and_persist() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(), provenance("first")).unwrap();
    let second = registry.create(definition(), provenance("second")).unwrap();
    assert_ne!(first.processor_id, second.processor_id);
    assert_eq!(first.processor_id.len(), "processor_".len() + 32);
    assert_eq!(first.version, second.version);
    assert_eq!(
        first.content_sha256,
        "0a9c8173319f8107f9ad2c37446de12021176145edc6c7fbe2862dfac4a9c657"
    );
    assert_eq!(first.version, format!("sha256:{}", first.content_sha256));
    assert_ne!(first.git_provenance, second.git_provenance);
    assert!(first.created_at_unix_ms > 0);
    assert_eq!(
        serde_json::to_value(&first.validation).unwrap(),
        json!({
            "syntax_checked": true,
            "types_checked": true,
            "supported_lowering_checked": true,
            "ddlog_compilation_performed": false
        })
    );
    drop(registry);
    let reopened = directory.registry();
    assert_eq!(reopened.get(&first.processor_id, None).unwrap(), first);
    assert_eq!(reopened.get(&second.processor_id, None).unwrap(), second);
}

#[test]
fn publish_checks_expected_pointer_and_pins_immutable_history() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(), provenance("first")).unwrap();
    let first_path = directory
        .0
        .join("registry")
        .join(&first.processor_id)
        .join("versions")
        .join(format!("{}.json", first.content_sha256));
    let original_bytes = fs::read(first_path).unwrap();
    let mut changed = definition();
    program(&mut changed).rules.push('\n');
    let second = registry
        .publish(
            &first.processor_id,
            changed,
            &first.version,
            provenance("second"),
        )
        .unwrap();
    assert_eq!(first.processor_id, second.processor_id);
    assert_ne!(first.version, second.version);
    assert_eq!(registry.get(&first.processor_id, None).unwrap(), second);
    assert_eq!(
        registry
            .get(&first.processor_id, Some(&first.version))
            .unwrap(),
        first
    );
    let error = registry
        .publish(&first.processor_id, definition(), &first.version, None)
        .unwrap_err();
    assert!(error.contains("Current version conflict"), "{error}");
    assert_eq!(registry.get(&first.processor_id, None).unwrap(), second);
    let back = registry
        .publish(
            &first.processor_id,
            definition(),
            &second.version,
            provenance("ignored"),
        )
        .unwrap();
    assert_eq!(back, first);
    assert_eq!(
        fs::read(
            directory
                .0
                .join("registry")
                .join(&first.processor_id)
                .join("versions")
                .join(format!("{}.json", first.content_sha256))
        )
        .unwrap(),
        original_bytes
    );
}

#[test]
fn fork_has_new_identity_and_exact_source_lineage() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(), provenance("source")).unwrap();
    let mut changed = definition();
    program(&mut changed).rules.push('\n');
    let second = registry
        .publish(&first.processor_id, changed.clone(), &first.version, None)
        .unwrap();
    let fork = registry
        .fork(&first.processor_id, &first.version, provenance("fork"))
        .unwrap();
    assert_ne!(fork.processor_id, first.processor_id);
    assert_eq!(fork.definition, first.definition);
    assert_eq!(fork.version, first.version);
    assert_eq!(fork.git_provenance, provenance("fork"));
    assert_eq!(
        fork.lineage,
        Some(ProcessorReference {
            processor_id: first.processor_id.clone(),
            version: first.version.clone()
        })
    );
    let updated_fork = registry
        .publish(&fork.processor_id, changed, &fork.version, None)
        .unwrap();
    assert_eq!(updated_fork.lineage, fork.lineage);
    assert_eq!(registry.get(&first.processor_id, None).unwrap(), second);
    assert_eq!(
        registry
            .get(&fork.processor_id, Some(&fork.version))
            .unwrap(),
        fork
    );
}

#[test]
fn exact_operation_binding_participates_in_definition_identity() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let ordinary = registry.create(definition(), None).unwrap();
    let mut bound = definition();
    program(&mut bound).operation = Some(RegisteredOperationBinding {
        name: "review".into(),
        version: "v1".into(),
        description: "Review text".into(),
    });
    let first = registry.create(bound.clone(), None).unwrap();
    assert_ne!(ordinary.version, first.version);
    program(&mut bound).operation.as_mut().unwrap().version = "v2".into();
    let version_change = registry.create(bound.clone(), None).unwrap();
    assert_ne!(version_change.version, first.version);
    program(&mut bound).operation.as_mut().unwrap().version = "v1".into();
    program(&mut bound)
        .operation
        .as_mut()
        .unwrap()
        .description
        .push_str(" with a different contract");
    let contract_change = registry.create(bound, None).unwrap();
    assert_ne!(contract_change.version, first.version);
    assert_eq!(registry.get(&first.processor_id, None).unwrap(), first);
}

#[test]
fn invalid_definitions_never_publish_or_advance_current() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(), None).unwrap();
    let processor_directory = directory.0.join("registry").join(&first.processor_id);
    let pointer_before = fs::read(processor_directory.join("current.json")).unwrap();
    let mut bad_type = definition();
    program(&mut bad_type).schemas["visible"]["fields"] = json!(["int"]);
    let mut unsupported_schema = definition();
    program(&mut unsupported_schema).schemas["item"]["fields"] = json!(["float"]);
    let mut invalid = vec![bad_type, unsupported_schema];
    for rules in [
        "visible(X :- item(X).",               // Invalid syntax.
        "visible(Y) :- item(X).",              // Unbound head variable.
        "visible(X) :- missing(X).",           // Undeclared relation.
        "visible(X) :- visible(X).",           // Unsupported recursion.
        "visible(X) :- item(X), !visible(X).", // Unsupported negation.
        "visible(\"literal\").",               // Facts are not authored rules.
    ] {
        let mut candidate = definition();
        program(&mut candidate).rules = rules.into();
        invalid.push(candidate);
    }
    for candidate in invalid {
        assert!(registry.create(candidate.clone(), None).is_err());
        assert!(registry
            .publish(&first.processor_id, candidate, &first.version, None)
            .is_err());
        assert_eq!(registry.get(&first.processor_id, None).unwrap(), first);
        assert_eq!(
            fs::read(processor_directory.join("current.json")).unwrap(),
            pointer_before
        );
        assert_eq!(
            fs::read_dir(directory.0.join("registry")).unwrap().count(),
            1
        );
        assert_eq!(
            fs::read_dir(processor_directory.join("versions"))
                .unwrap()
                .count(),
            1
        );
    }
}

#[test]
fn registered_validation_supports_public_results_and_rejects_private_protocol() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let valid = ProcessorDefinition::Program(ProgramDefinition {
        rules: "reviewed(E,R,O) :- agent_result(E,R,O).".into(),
        schemas: json!({"reviewed":{"input":false,"fields":["string","int","string"]}}),
        operation: Some(RegisteredOperationBinding {
            name: "review".into(),
            version: "v1".into(),
            description: "Review text".into(),
        }),
        interface: None,
        operators: Vec::new(),
    });
    let first = registry.create(valid.clone(), None).unwrap();
    assert!(first.validation.syntax_checked && first.validation.types_checked);
    assert!(first.validation.supported_lowering_checked);
    assert!(!first.validation.ddlog_compilation_performed);
    let mut bad_type = valid.clone();
    program(&mut bad_type).schemas["reviewed"]["fields"] = json!(["string", "string", "string"]);
    let mut redeclared = valid.clone();
    program(&mut redeclared).schemas["agent_result"] =
        json!({"input":false,"fields":["string","int","string"]});
    let mut invalid = vec![bad_type, redeclared];
    for rules in [
        "agent_result(E,R,O) :- reviewed(E,R,O).",
        "reviewed(E,R,O) :- agent_intent(E,E,R,O).",
        "reviewed(E,R,O) :- agent_result(E,R,O), !agent_response(E,O).",
    ] {
        let mut candidate = valid.clone();
        program(&mut candidate).rules = rules.into();
        invalid.push(candidate);
    }
    for candidate in invalid {
        assert!(registry.create(candidate.clone(), None).is_err());
        assert!(registry
            .publish(&first.processor_id, candidate, &first.version, None)
            .is_err());
        assert_eq!(registry.get(&first.processor_id, None).unwrap(), first);
    }
    let mut next = valid;
    program(&mut next).rules.push('\n');
    let second = registry
        .publish(&first.processor_id, next, &first.version, None)
        .unwrap();
    assert_ne!(second.version, first.version);
    assert_eq!(second.validation, first.validation);
}

#[test]
fn malformed_or_tampered_records_fail_closed() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(), None).unwrap();
    assert!(registry.get("../registry", None).is_err());
    assert!(registry
        .get(&first.processor_id, Some("../../current"))
        .is_err());
    let file = directory
        .0
        .join("registry")
        .join(&first.processor_id)
        .join("versions")
        .join(format!("{}.json", first.content_sha256));
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    record["definition"]["rules"] = json!("tampered");
    fs::write(&file, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(registry
        .get(&first.processor_id, None)
        .unwrap_err()
        .contains("hash mismatch"));
    assert!(registry
        .publish(&first.processor_id, definition(), &first.version, None)
        .is_err());
    let mut invalid = definition();
    program(&mut invalid).schemas =
        json!({"item":{"input":true,"fields":["string"],"ignored":true}});
    assert!(registry.create(invalid, None).is_err());
    assert!(serde_json::from_value::<ProcessorDefinition>(
        json!({"rules":"","schemas":{},"unexpected":true})
    )
    .is_err());
}

#[test]
fn private_permissions_and_stale_lock_are_enforced() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(), None).unwrap();
    let root = directory.0.join("registry");
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let current = root.join(&first.processor_id).join("current.json");
    assert_eq!(
        fs::metadata(&current).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::write(
        root.join(".update.lock"),
        "pid=dead-operator-must-reconcile\n",
    )
    .unwrap();
    let error = registry
        .publish(&first.processor_id, definition(), &first.version, None)
        .unwrap_err();
    assert!(error.contains("operator reconciliation"), "{error}");
    assert!(registry.create(definition(), None).is_err());
    assert!(registry
        .fork(&first.processor_id, &first.version, None)
        .is_err());
    assert!(root.join(".update.lock").exists());
    assert_eq!(registry.get(&first.processor_id, None).unwrap(), first);
    let public = directory.0.join("public");
    fs::create_dir(&public).unwrap();
    fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(ProcessorRegistry::open(public)
        .unwrap_err()
        .contains("private"));
    let symlink = directory.0.join("symlink");
    std::os::unix::fs::symlink(&root, &symlink).unwrap();
    assert!(ProcessorRegistry::open(symlink)
        .unwrap_err()
        .contains("symbolic link"));
}

fn wait_for(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn subprocess_publish() {
    let Ok(root) = std::env::var("LEMMALOG_REGISTRY_TEST_ROOT") else {
        return;
    };
    let name = std::env::var("LEMMALOG_REGISTRY_TEST_NAME").unwrap();
    let processor = std::env::var("LEMMALOG_REGISTRY_TEST_PROCESSOR").unwrap();
    let expected = std::env::var("LEMMALOG_REGISTRY_TEST_VERSION").unwrap();
    let root = PathBuf::from(root);
    let registry = ProcessorRegistry::open(root.join("registry")).unwrap();
    fs::write(root.join(format!("ready-{name}")), "ready").unwrap();
    wait_for(&root.join("start"));
    let mut changed = definition();
    program(&mut changed)
        .rules
        .push_str(if name == "a" { "\n" } else { "\n\n" });
    match registry.publish(&processor, changed, &expected, None) {
        Ok(record) => println!("PUBLISHED:{}", record.version),
        Err(error) => {
            assert!(
                error.contains("Current version conflict") || error.contains("update lock exists"),
                "{error}"
            );
            println!("REJECTED:{error}");
        }
    }
}

#[test]
fn independent_processes_cannot_both_advance_the_same_expected_pointer() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(), None).unwrap();
    let launch = |name: &str| {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "subprocess_publish", "--nocapture"])
            .env("LEMMALOG_REGISTRY_TEST_ROOT", &directory.0)
            .env("LEMMALOG_REGISTRY_TEST_NAME", name)
            .env("LEMMALOG_REGISTRY_TEST_PROCESSOR", &first.processor_id)
            .env("LEMMALOG_REGISTRY_TEST_VERSION", &first.version)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let a = launch("a");
    let b = launch("b");
    wait_for(&directory.0.join("ready-a"));
    wait_for(&directory.0.join("ready-b"));
    fs::write(directory.0.join("start"), "go").unwrap();
    let outputs = [a.wait_with_output().unwrap(), b.wait_with_output().unwrap()];
    let mut published = Vec::new();
    let mut rejected = 0;
    for output in outputs {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        for line in stdout.lines() {
            if let Some(version) = line.strip_prefix("PUBLISHED:") {
                published.push(version.to_string());
            }
            if line.starts_with("REJECTED:") {
                rejected += 1;
            }
        }
    }
    assert_eq!(published.len(), 1);
    assert_eq!(rejected, 1);
    assert_eq!(
        registry.get(&first.processor_id, None).unwrap().version,
        published[0]
    );
    assert_eq!(
        registry
            .get(&first.processor_id, Some(&first.version))
            .unwrap(),
        first
    );
    assert!(!directory.0.join("registry/.update.lock").exists());
}

#[test]
fn discovery_pages_by_identity_and_searches_literal_current_definitions() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let mut records = Vec::new();
    for name in ["MiXeD", "other", "third", "fourth", "fifth"] {
        let mut candidate = definition();
        program(&mut candidate).rules = format!("visible(X) :- item(X), X = \"{name}\".");
        records.push(registry.create(candidate, None).unwrap());
    }
    let mut expected: Vec<_> = records.iter().map(|row| row.processor_id.clone()).collect();
    expected.sort();
    let mut observed = Vec::new();
    let mut cursor = None;
    loop {
        let page = registry.list(2, cursor.as_deref(), false).unwrap();
        assert!((1..=2).contains(&page.processors.len()));
        assert!(page
            .processors
            .iter()
            .all(|row| row.status == ProcessorStatus::Active));
        if let Some(previous) = &cursor {
            assert!(page
                .processors
                .iter()
                .all(|row| &row.processor_id > previous));
        }
        if let Some(next) = &page.next_cursor {
            assert_eq!(next, &page.processors.last().unwrap().processor_id);
        }
        observed.extend(page.processors.into_iter().map(|row| row.processor_id));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(observed, expected);
    let result = registry.search("mIxEd", 10, None, false).unwrap();
    assert_eq!(result.processors.len(), 1);
    assert_eq!(result.processors[0].processor_id, records[0].processor_id);
    assert_eq!(
        registry
            .search(&records[0].processor_id.to_uppercase(), 10, None, false)
            .unwrap()
            .processors
            .len(),
        1
    );
    assert_eq!(
        registry
            .search(&records[0].version.to_uppercase(), 10, None, false)
            .unwrap()
            .processors
            .len(),
        1
    );
    assert!(registry
        .search(".*", 10, None, false)
        .unwrap()
        .processors
        .is_empty());
    assert_eq!(
        registry.search("", 2, None, false).unwrap(),
        registry.list(2, None, false).unwrap()
    );
    assert!(registry.list(0, None, false).is_err());
    assert!(registry.list(101, None, false).is_err());
    assert!(registry.list(1, Some("../outside"), false).is_err());
    assert!(registry
        .search("mixed", 1, Some("not-a-cursor"), false)
        .is_err());
    let changed = registry
        .publish(
            &records[0].processor_id,
            definition(),
            &records[0].version,
            None,
        )
        .unwrap();
    assert!(registry
        .search("mixed", 10, None, false)
        .unwrap()
        .processors
        .is_empty());
    assert_eq!(
        registry
            .get(&records[0].processor_id, Some(&records[0].version))
            .unwrap(),
        records[0]
    );
    let mut operation = definition();
    program(&mut operation).operation = Some(RegisteredOperationBinding {
        name: "FindThisOperation".into(),
        version: "v1".into(),
        description: "Review".into(),
    });
    let operation = registry.create(operation, None).unwrap();
    assert_eq!(
        registry
            .search("findthisoperation", 10, None, false)
            .unwrap()
            .processors[0]
            .processor_id,
        operation.processor_id
    );
    assert_eq!(
        directory
            .registry()
            .get(&changed.processor_id, None)
            .unwrap(),
        changed
    );
}

#[test]
fn archive_is_conditional_idempotent_and_keeps_every_version_readable() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(), None).unwrap();
    let mut changed = definition();
    program(&mut changed).rules.push('\n');
    let second = registry
        .publish(&first.processor_id, changed, &first.version, None)
        .unwrap();
    let path = directory.0.join("registry").join(&first.processor_id);
    let versions: Vec<_> = [&first, &second]
        .into_iter()
        .map(|record| {
            let path = path
                .join("versions")
                .join(format!("{}.json", record.content_sha256));
            (path.clone(), fs::read(path).unwrap())
        })
        .collect();
    assert!(registry
        .archive(&first.processor_id, &first.version, 0)
        .unwrap_err()
        .contains("version conflict"));
    assert!(!path.join("lifecycle/current.json").exists());
    let archive = registry
        .archive(&first.processor_id, &second.version, 0)
        .unwrap();
    assert_eq!(archive.status, ProcessorStatus::Archived);
    assert_eq!(archive.version, second.version);
    assert!(archive.changed_at_unix_ms.unwrap() > 0);
    assert_eq!(
        registry
            .archive(&first.processor_id, &second.version, 1)
            .unwrap(),
        archive
    );
    assert!(registry
        .archive(&first.processor_id, &first.version, 0)
        .is_err());
    assert!(registry
        .get(&first.processor_id, None)
        .unwrap_err()
        .contains("archived"));
    assert!(registry.ensure_active(&first.processor_id).is_err());
    assert!(registry
        .publish(&first.processor_id, definition(), &second.version, None)
        .unwrap_err()
        .contains("archived"));
    assert!(registry
        .fork(&first.processor_id, &first.version, None)
        .unwrap_err()
        .contains("archived"));
    assert!(registry
        .list(10, None, false)
        .unwrap()
        .processors
        .is_empty());
    assert!(registry
        .search(&first.processor_id, 10, None, false)
        .unwrap()
        .processors
        .is_empty());
    let page = registry.list(10, None, true).unwrap();
    assert_eq!(page.processors.len(), 1);
    assert_eq!(page.processors[0].status, ProcessorStatus::Archived);
    assert_eq!(
        page.processors[0].archived_at_unix_ms,
        archive.changed_at_unix_ms
    );
    assert_eq!(page.processors[0].version, second.version);
    assert_eq!(
        registry
            .search(&first.processor_id, 10, None, true)
            .unwrap(),
        page
    );
    let reopened = directory.registry();
    assert_eq!(
        reopened
            .archive(&first.processor_id, &second.version, 1)
            .unwrap(),
        archive
    );
    for record in [&first, &second] {
        assert_eq!(
            reopened
                .get(&record.processor_id, Some(&record.version))
                .unwrap(),
            *record
        );
    }
    for (path, bytes) in versions {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

// Lifecycle acceptance is defined before the restore implementation. Definition
// version CAS and lifecycle revision CAS are separate; archive/restore never
// compile, replace definitions, or make an old lifecycle request valid again.
#[test]
fn restore_round_trip_preserves_definitions_and_rejects_stale_aba_requests() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(), None).unwrap();
    let path = directory.0.join("registry").join(&first.processor_id);
    let original_pointer = fs::read(path.join("current.json")).unwrap();
    let original_version = fs::read(
        path.join("versions")
            .join(format!("{}.json", first.content_sha256)),
    )
    .unwrap();
    assert_eq!(
        registry.list(10, None, false).unwrap().processors[0].lifecycle_revision,
        0
    );
    let already_active = registry
        .restore(&first.processor_id, &first.version, 0)
        .unwrap();
    assert_eq!(already_active.status, ProcessorStatus::Active);
    assert_eq!(already_active.lifecycle_revision, 0);
    assert_eq!(already_active.changed_at_unix_ms, None);
    assert!(!path.join("lifecycle/current.json").exists());

    let archived = registry
        .archive(&first.processor_id, &first.version, 0)
        .unwrap();
    assert_eq!(archived.lifecycle_revision, 1);
    assert_eq!(
        registry
            .archive(&first.processor_id, &first.version, 1)
            .unwrap(),
        archived
    );
    let stale = registry
        .archive(&first.processor_id, &first.version, 0)
        .unwrap_err();
    for expected in ["expected 0", "current 1", "latest", "reconsider"] {
        assert!(stale.contains(expected), "{stale}");
    }
    let archived_error = registry.ensure_active(&first.processor_id).unwrap_err();
    assert!(
        archived_error.contains("archived")
            && archived_error.contains("restore")
            && archived_error.contains("include_archived")
    );
    assert!(registry
        .list(10, None, false)
        .unwrap()
        .processors
        .is_empty());
    assert_eq!(
        registry.list(10, None, true).unwrap().processors[0].lifecycle_revision,
        1
    );
    let archived_bytes =
        fs::read(path.join("lifecycle/revisions/00000000000000000001.json")).unwrap();

    let restored = registry
        .restore(&first.processor_id, &first.version, 1)
        .unwrap();
    assert_eq!(restored.status, ProcessorStatus::Active);
    assert_eq!(restored.lifecycle_revision, 2);
    assert_eq!(
        registry
            .restore(&first.processor_id, &first.version, 2)
            .unwrap(),
        restored
    );
    assert!(registry
        .archive(&first.processor_id, &first.version, 0)
        .unwrap_err()
        .contains("current 2"));
    assert!(registry
        .restore(&first.processor_id, &first.version, 1)
        .unwrap_err()
        .contains("current 2"));
    registry.ensure_active(&first.processor_id).unwrap();
    assert_eq!(registry.get(&first.processor_id, None).unwrap(), first);
    assert_eq!(
        registry
            .get(&first.processor_id, Some(&first.version))
            .unwrap(),
        first
    );
    assert_eq!(
        registry
            .search(&first.processor_id, 10, None, false)
            .unwrap()
            .processors[0]
            .lifecycle_revision,
        2
    );
    assert_eq!(
        fs::read(path.join("current.json")).unwrap(),
        original_pointer
    );
    assert_eq!(
        fs::read(
            path.join("versions")
                .join(format!("{}.json", first.content_sha256))
        )
        .unwrap(),
        original_version
    );
    assert_eq!(
        fs::read(path.join("lifecycle/revisions/00000000000000000001.json")).unwrap(),
        archived_bytes
    );
    assert_eq!(
        fs::read_dir(path.join("lifecycle/revisions"))
            .unwrap()
            .count(),
        2
    );
    assert_eq!(
        directory
            .registry()
            .restore(&first.processor_id, &first.version, 2)
            .unwrap(),
        restored
    );

    let mut next = definition();
    program(&mut next).rules.push('\n');
    let next = registry
        .publish(&first.processor_id, next, &first.version, None)
        .unwrap();
    assert_eq!(
        registry.list(10, None, false).unwrap().processors[0].lifecycle_revision,
        2
    );
    let no_op_after_publish = registry
        .restore(&first.processor_id, &next.version, 2)
        .unwrap();
    assert_eq!(no_op_after_publish.version, next.version);
    assert_eq!(no_op_after_publish.lifecycle_revision, 2);
    assert_eq!(
        no_op_after_publish.changed_at_unix_ms,
        restored.changed_at_unix_ms
    );
    let stored_restore: serde_json::Value = serde_json::from_slice(
        &fs::read(path.join("lifecycle/revisions/00000000000000000002.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stored_restore["version"], first.version);
    assert_eq!(
        fs::read_dir(path.join("lifecycle/revisions"))
            .unwrap()
            .count(),
        2
    );
    let stale_version = registry
        .archive(&first.processor_id, &first.version, 2)
        .unwrap_err();
    assert!(
        stale_version.contains(&first.version)
            && stale_version.contains(&next.version)
            && stale_version.contains("latest")
    );
    assert_eq!(
        registry
            .archive(&first.processor_id, &next.version, 2)
            .unwrap()
            .lifecycle_revision,
        3
    );
}

#[test]
fn unknown_registry_references_provide_discovery_actions() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let unknown = format!("processor_{}", "f".repeat(32));
    let error = registry.get(&unknown, None).unwrap_err();
    assert!(
        error.contains(&unknown) && error.contains("list") && error.contains("search"),
        "{error}"
    );
    let record = registry.create(definition(), None).unwrap();
    let missing = format!("sha256:{}", "0".repeat(64));
    let error = registry
        .get(&record.processor_id, Some(&missing))
        .unwrap_err();
    assert!(
        error.contains(&record.processor_id) && error.contains(&missing) && error.contains("get"),
        "{error}"
    );
}

#[test]
fn initial_lifecycle_noop_has_no_transition_time_even_after_code_publication() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(), None).unwrap();
    let initial = registry
        .restore(&first.processor_id, &first.version, 0)
        .unwrap();
    assert_eq!(initial.lifecycle_revision, 0);
    assert_eq!(initial.changed_at_unix_ms, None);
    let mut next = definition();
    program(&mut next).rules.push('\n');
    let next = registry
        .publish(&first.processor_id, next, &first.version, None)
        .unwrap();
    let unchanged = registry
        .restore(&first.processor_id, &next.version, 0)
        .unwrap();
    assert_eq!(unchanged.version, next.version);
    assert_eq!(unchanged.lifecycle_revision, 0);
    assert_eq!(unchanged.changed_at_unix_ms, None);
    assert!(!directory
        .0
        .join("registry")
        .join(&first.processor_id)
        .join("lifecycle/current.json")
        .exists());
}

#[test]
fn subprocess_lifecycle_transition() {
    let Ok(root) = std::env::var("LEMMALOG_LIFECYCLE_TEST_ROOT") else {
        return;
    };
    let name = std::env::var("LEMMALOG_LIFECYCLE_TEST_NAME").unwrap();
    let processor = std::env::var("LEMMALOG_LIFECYCLE_TEST_PROCESSOR").unwrap();
    let version = std::env::var("LEMMALOG_LIFECYCLE_TEST_VERSION").unwrap();
    let root = PathBuf::from(root);
    let registry = ProcessorRegistry::open(root.join("registry")).unwrap();
    fs::write(root.join(format!("ready-{name}")), "ready").unwrap();
    wait_for(&root.join("start"));
    match registry.archive(&processor, &version, 0) {
        Ok(receipt) => println!("TRANSITION:{}", receipt.lifecycle_revision),
        Err(error) => {
            assert!(
                error.contains("revision conflict") || error.contains("update lock exists"),
                "{error}"
            );
            println!("REJECTED:{error}");
        }
    }
}

#[test]
fn independent_processes_cannot_both_commit_one_lifecycle_revision() {
    let directory = TestDirectory::new();
    let registry = directory.registry();
    let first = registry.create(definition(), None).unwrap();
    let launch = |name: &str| {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "subprocess_lifecycle_transition", "--nocapture"])
            .env("LEMMALOG_LIFECYCLE_TEST_ROOT", &directory.0)
            .env("LEMMALOG_LIFECYCLE_TEST_NAME", name)
            .env("LEMMALOG_LIFECYCLE_TEST_PROCESSOR", &first.processor_id)
            .env("LEMMALOG_LIFECYCLE_TEST_VERSION", &first.version)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let a = launch("a");
    let b = launch("b");
    wait_for(&directory.0.join("ready-a"));
    wait_for(&directory.0.join("ready-b"));
    fs::write(directory.0.join("start"), "go").unwrap();
    let mut committed = 0;
    let mut rejected = 0;
    for output in [a.wait_with_output().unwrap(), b.wait_with_output().unwrap()] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        for line in String::from_utf8(output.stdout).unwrap().lines() {
            if line == "TRANSITION:1" {
                committed += 1;
            }
            if line.starts_with("REJECTED:") {
                rejected += 1;
            }
        }
    }
    assert_eq!((committed, rejected), (1, 1));
    let restored = registry
        .restore(&first.processor_id, &first.version, 1)
        .unwrap();
    assert_eq!(restored.lifecycle_revision, 2);
    assert!(registry
        .archive(&first.processor_id, &first.version, 0)
        .is_err());
    assert_eq!(
        registry.list(10, None, false).unwrap().processors[0].lifecycle_revision,
        2
    );
}
