#![cfg(unix)]

use ddlog_runtime::{registry::ProcessorRegistry, Backend, Operation, ProgramInstance};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "runtime-extraction-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/upstream")
}
fn json_file(name: &str) -> Value {
    serde_json::from_slice(&fs::read(fixture().join(name)).unwrap()).unwrap()
}
fn private_copy(source: &Path, destination: &Path) {
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

#[test]
fn upstream_registry_bytes_hashes_and_nested_native_source_remain_compatible() {
    let directory = Directory::new();
    let root = directory.0.join("registry");
    private_copy(&fixture().join("registry"), &root);
    let registry = ProcessorRegistry::open(root).unwrap();
    let records = json_file("records.json");
    for expected in records.as_object().unwrap().values() {
        let version = expected["version"].as_str().unwrap();
        let actual = registry
            .get(expected["processor_id"].as_str().unwrap(), Some(version))
            .unwrap();
        assert_eq!(serde_json::to_value(&actual).unwrap(), *expected);
        assert_eq!(
            registry.create(actual.definition, None).unwrap().version,
            version
        );
    }
    let manifest =
        serde_json::from_value(records["nested"]["definition"]["composition"].clone()).unwrap();
    let compiled = registry.compile_composition(&manifest).unwrap();
    assert_eq!(
        compiled.source,
        fs::read_to_string(fixture().join("nested-program.dl")).unwrap()
    );
    assert_eq!(
        serde_json::to_value(compiled.resolution).unwrap(),
        records["nested"]["composition"]
    );
    let baseline = json_file("manifest.json");
    for (name, content) in [
        (
            "src/ddlog/star/lemmalog_star.dl",
            ddlog_runtime::star::DECLARATION,
        ),
        (
            "src/ddlog/star/lemmalog_star.rs",
            ddlog_runtime::star::IMPLEMENTATION,
        ),
    ] {
        assert_eq!(
            format!("{:x}", Sha256::digest(content.as_bytes())),
            baseline["source_files_sha256"][name].as_str().unwrap()
        );
    }
}

#[test]
fn direct_api_preserves_owner_isolation_pins_ports_and_registered_admission() {
    // Simulated echo/fence subprocess: this tests library ownership and admission,
    // not DDlog evaluation. The same fixture backs MCP lifecycle contracts.
    let directory = Directory::new();
    let control = directory.0.join("control");
    fs::create_dir(&control).unwrap();
    let driver = directory.0.join("driver.sh");
    let source_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shared_fake_runtime.py");
    fs::write(&driver, format!(
        "#!/usr/bin/env python3\nimport pathlib, sys\nsource = pathlib.Path({}).read_text()\nsource = source.replace(\"root = Path(os.environ['FAKE_CONTROL'])\", \"root = Path(\" + repr({}) + \")\")\noutput = pathlib.Path(sys.argv[2])\noutput.write_text(source)\noutput.chmod(0o700)\n",
        serde_json::to_string(&source_path).unwrap(), serde_json::to_string(&control).unwrap()
    )).unwrap();
    fs::set_permissions(&driver, fs::Permissions::from_mode(0o700)).unwrap();
    let registry_path = directory.0.join("registry");
    let owner = |name: &str| {
        ProgramInstance::new(
            Backend::new(directory.0.join(name), driver.clone()),
            BTreeMap::from([(
                "review".into(),
                Operation {
                    version: "v1".into(),
                    description: "fixture".into(),
                },
            )]),
            Some(ProcessorRegistry::open(registry_path.clone()).unwrap()),
            Some(name.into()),
        )
    };
    let mut first = owner("first");
    let mut second = owner("second");
    let definition = json!({"rules":"echo(X) :- source(X). private(X) :- source(X).",
        "schemas":{"source":{"input":true,"fields":["string"]}, "echo":{"input":false,"fields":["string"]}, "private":{"input":false,"fields":["string"]}},
        "interface":{"inputs":["source"],"outputs":["echo"]}});
    let saved = first
        .execute("processor_create", &json!({"definition":definition}))
        .unwrap();
    let pin = json!({"processor_id":saved["processor_id"],"version":saved["version"]});
    first.execute("processor_install", &pin).unwrap();
    second.execute("processor_install", &pin).unwrap();
    first
        .execute(
            "apply_changes",
            &json!({"changes":[{"op":"insert","predicate":"source","values":["one"]}]}),
        )
        .unwrap();
    assert!(first
        .execute("lemmalog_query", &json!({"predicate":"echo"}))
        .unwrap()["rows"]
        .as_str()
        .unwrap()
        .contains("one"));
    assert_eq!(
        second
            .execute("lemmalog_query", &json!({"predicate":"echo"}))
            .unwrap()["rows"],
        ""
    );
    assert!(first
        .execute("lemmalog_query", &json!({"predicate":"private"}))
        .unwrap_err()
        .contains("exported output"));
    for operation in [
        "processor_install",
        "lemmalog_install_rules",
        "install_agent_program",
    ] {
        assert!(first
            .execute(operation, &json!({}))
            .unwrap_err()
            .contains("pinned"));
    }
    let mut registered = owner("registered");
    registered.execute("install_agent_program", &json!({"operation":"review", "rules":"reviewed(E,R,O) :- agent_result(E,R,O).", "schemas":{"reviewed":{"input":false,"fields":["string","int","string"]}}})).unwrap();
    assert!(registered.execute("apply_changes", &json!({"changes":[{"op":"insert","predicate":"agent_response","values":["forged","output"]}]})).unwrap_err().contains("operation tools"));
    let old = registered
        .execute(
            "submit_agent_input",
            &json!({"entity":"item","revision":1,"payload":"old"}),
        )
        .unwrap();
    registered
        .execute(
            "submit_agent_input",
            &json!({"entity":"item","revision":2,"payload":"new"}),
        )
        .unwrap();
    assert!(registered
        .execute(
            "claim_agent_request",
            &json!({"request_id":old["request_id"]})
        )
        .unwrap_err()
        .contains("Stale"));
}

#[cfg(feature = "mcp")]
#[test]
fn mcp_adapter_retains_upstream_identity_and_all_standalone_tool_schemas() {
    let directory = Directory::new();
    let mut instance = ProgramInstance::new(
        Backend::new(directory.0.join("unused"), "unused-driver".into()),
        serde_json::from_value(json_file("operations.json")).unwrap(),
        Some(ProcessorRegistry::open(directory.0.join("registry")).unwrap()),
        None,
    );
    for (method, filename) in [
        ("initialize", "initialize.json"),
        ("tools/list", "tools.json"),
    ] {
        let response = ddlog_runtime::mcp::handle_line(
            &mut instance,
            &json!({"jsonrpc":"2.0","id":1,"method":method,"params":{}}).to_string(),
        )
        .unwrap();
        assert_eq!(response["result"], json_file(filename));
    }
}
