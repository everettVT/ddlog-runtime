//! Runtime-owned inventory of explicitly created worlds. No host-process discovery.
//!
//! The manager owns existing `ProgramInstance`s; attaching a client must reuse this
//! manager, never instantiate a second manager as a discovery mechanism.
use crate::instance::{public_relations, PublicRelation};
use crate::registry::{
    kind_of, GitProvenance, ImportStatus, ProcessorDefinition, ProcessorReference,
    ProcessorRegistry, ProcessorVersion,
};
use crate::telemetry::{spawn_tailer, Reader, State};
use crate::{Backend, ObserverOptions, ProgramInstance};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: u32 = 1;
/// Implicit library holding every registered definition without an explicit one.
pub const UNASSIGNED_LIBRARY: &str = "unassigned";
const BUILD_LOG_TAIL_LINES: usize = 60;
/// Owner lock acquisition retries a released-but-not-yet-visible lock for
/// about 100 ms before reporting an existing owner.
const OWNER_LOCK_ATTEMPTS: u32 = 10;
const OWNER_LOCK_RETRY_MS: u64 = 10;
/// Saved source-library registration. This does not claim executable definitions.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryDefinition {
    pub name: String,
    pub repository: String,
    pub revision: String,
}
/// One input mutation of a scenario, addressed by public relation name.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScenarioChange {
    pub op: String,
    pub predicate: String,
    pub values: Vec<Value>,
}
/// Cumulative test step: apply `changes`, then every relation in `expect` must
/// equal the listed rows as a set. Validated against the definition's public
/// relations when stored and again when a test world is created.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub changes: Vec<ScenarioChange>,
    pub expect: BTreeMap<String, Vec<Vec<Value>>>,
}
fn default_purpose() -> String {
    "instance".into()
}
fn is_instance(purpose: &str) -> bool {
    purpose == "instance"
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldDefinition {
    pub label: String,
    pub processor: ProcessorReference,
    /// `instance` (default) or `test`; test worlds run their scenarios after install.
    #[serde(default = "default_purpose", skip_serializing_if = "is_instance")]
    pub purpose: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scenarios: Vec<Scenario>,
}
/// Message from a start thread: a live instance, a completed test run whose
/// instance was dropped, or the install/test failure.
type StartOutcome = Result<Option<ProgramInstance>, String>;
/// The capture tailer of one generation. `live` tells it whether an instance
/// (or a pending start) still exists; without one it exits after idle polls.
/// Dropping a tailer stops and joins its thread, so no thread outlives its
/// world (a test world removed after a failed start, a manager going away).
struct Tailer {
    stop: Arc<AtomicBool>,
    live: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}
impl Tailer {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
impl Drop for Tailer {
    fn drop(&mut self) {
        self.stop();
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct World {
    schema_version: u32,
    definition: WorldDefinition,
    #[serde(skip)]
    instance: Option<ProgramInstance>,
    state: String,
    error: Option<String>,
    generation: u64,
    metadata: Option<crate::inspection::InspectionMetadata>,
    /// Ingested capture of the current generation; shared with its tailer.
    #[serde(skip)]
    telemetry: Option<Arc<Mutex<State>>>,
    #[serde(skip)]
    tailer: Option<Tailer>,
    /// Recovered worlds read their last capture once, on the first full status.
    #[serde(skip)]
    reader: Option<Reader>,
    /// Generation whose compiled topology has been retained (A8), once each.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capture_generation: Option<u64>,
    history: Vec<Value>,
    #[serde(skip)]
    pending: Option<std::sync::mpsc::Receiver<StartOutcome>>,
    #[serde(skip)]
    stop_requested: bool,
    /// Last observed scenario run of a test world; copied from the start thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    test: Option<Value>,
    #[serde(skip)]
    test_progress: Option<Arc<Mutex<Value>>>,
}
/// Filter and shape of an `inventory` reply.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryQuery {
    #[serde(default)]
    pub processor_id: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    /// Omit `inspection`, `instance` and `managed_processes` and never call the
    /// live instance; this is what a sidebar polls.
    #[serde(default)]
    pub summary: bool,
}
/// `import` verb arguments.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    pub source_registry: PathBuf,
    #[serde(default)]
    pub processor_id: Option<String>,
    #[serde(default)]
    pub library_id: Option<String>,
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    #[serde(default)]
    pub dry_run: bool,
}
/// `register` verb arguments: every registered definition carries a name.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub library_id: Option<String>,
    pub definition: Value,
    #[serde(default)]
    pub git_provenance: Option<GitProvenance>,
}
/// `test` verb arguments.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestRequest {
    pub processor_id: String,
    pub version: String,
    #[serde(default)]
    pub scenarios: Option<Vec<String>>,
    #[serde(default)]
    pub keep_world: bool,
}
/// One embedding owner. Dropping it stops all its worlds. Registry definitions
/// survive, but live inventory does not automatically recover across owner exit.
pub struct WorldManager {
    registry_root: PathBuf,
    build_root: PathBuf,
    driver: PathBuf,
    worlds: BTreeMap<String, World>,
    shutdown: WorldShutdown,
    _owner_lock: std::fs::File,
}
impl WorldManager {
    pub fn new(
        registry_root: PathBuf,
        build_root: PathBuf,
        driver: PathBuf,
    ) -> Result<Self, String> {
        if !build_root.is_absolute() || !driver.is_absolute() || !registry_root.is_absolute() {
            return Err(
                "Registry, build root and driver must be absolute operator-configured paths".into(),
            );
        }
        ProcessorRegistry::open(registry_root.clone())?;
        // Reuse the registry's same-user private-directory checks.
        ProcessorRegistry::open(build_root.clone())?;
        let owner_lock = lock_owner(&build_root)?;
        let mut worlds = BTreeMap::new();
        for entry in std::fs::read_dir(&build_root).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let id = entry.file_name().to_string_lossy().into_owned();
            if !id.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                continue;
            }
            if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                return Err("Invalid world directory".into());
            }
            let path = entry.path().join("world.json");
            if !path.exists() {
                continue;
            }
            let mut world: World =
                serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if world.schema_version != SCHEMA_VERSION {
                return Err("Unsupported world record schema".into());
            }
            if !matches!(
                world.state.as_str(),
                "created"
                    | "starting"
                    | "running"
                    | "stopping"
                    | "stopped"
                    | "failed"
                    | "interrupted"
            ) {
                return Err("Invalid persisted world state".into());
            }
            if matches!(world.state.as_str(), "starting" | "running" | "stopping") {
                world.state = "interrupted".into();
                world.error =
                    Some("Previous owner exited; no process was adopted or restarted".into());
                persist(&build_root, &id, &mut world)?;
            }
            if world.generation > 0 {
                world.telemetry = Some(Arc::new(Mutex::new(State::default())));
                world.reader = Some(Reader::new(
                    entry
                        .path()
                        .join(world.generation.to_string())
                        .join("native-events.jsonl"),
                ));
            }
            worlds.insert(id, world);
        }
        Ok(Self {
            registry_root,
            build_root,
            driver,
            worlds,
            _owner_lock: owner_lock,
            shutdown: WorldShutdown::default(),
        })
    }
    pub fn shutdown_handle(&self) -> WorldShutdown {
        self.shutdown.clone()
    }
    /// The persisted catalog with every entry normalized to carry `processors`.
    fn stored_catalog(&self) -> Result<Value, String> {
        let path = self.build_root.join("libraries.json");
        let mut catalog = if path.exists() {
            let value: Value =
                serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if value["schema_version"] != 1 || !value["libraries"].is_array() {
                return Err("Invalid library catalog".into());
            }
            value
        } else {
            json!({"schema_version":1,"libraries":[]})
        };
        for entry in catalog["libraries"].as_array_mut().unwrap() {
            if !entry["processors"].is_array() {
                entry["processors"] = json!([]);
            }
        }
        Ok(catalog)
    }
    /// Full library entries including their registered definitions, plus the
    /// implicit `unassigned` library (name "Unassigned"), always listed last.
    pub fn libraries(&self) -> Result<Value, String> {
        let mut catalog = self.stored_catalog()?;
        let libraries = catalog["libraries"].as_array_mut().unwrap();
        let unassigned = libraries
            .iter()
            .position(|entry| entry["id"] == UNASSIGNED_LIBRARY)
            .map(|index| libraries.remove(index))
            .unwrap_or_else(|| json!({"id":UNASSIGNED_LIBRARY,"name":"Unassigned","repository":null,"revision":null,"registered_at_unix_ms":null,"definitions":"registered_separately","provenance":"implicit","processors":[]}));
        libraries.push(unassigned);
        Ok(catalog)
    }
    /// Move one (processor, version) into `library_id`, naming it. A pin belongs
    /// to exactly one library; the implicit library is created on first use.
    pub fn associate(
        &self,
        processor_id: &str,
        version: &str,
        library_id: &str,
        name: &str,
        description: &str,
    ) -> Result<Value, String> {
        validate_name_description(name, description)?;
        let mut catalog = self.libraries()?;
        let libraries = catalog["libraries"].as_array_mut().unwrap();
        if !libraries.iter().any(|entry| entry["id"] == library_id) {
            return Err(format!(
                "Unknown library {library_id}; call library_create first"
            ));
        }
        for entry in libraries.iter_mut() {
            let processors = entry["processors"].as_array_mut().unwrap();
            processors.retain(|p| !(p["processor_id"] == processor_id && p["version"] == version));
        }
        let association = json!({"processor_id":processor_id,"version":version,"name":name,"description":description});
        libraries
            .iter_mut()
            .find(|entry| entry["id"] == library_id)
            .unwrap()["processors"]
            .as_array_mut()
            .unwrap()
            .push(association.clone());
        atomic_json(&self.build_root.join("libraries.json"), &catalog)?;
        Ok(association)
    }
    /// Library id, name and description of one pin; derived when unassociated.
    fn association(&self, record: &ProcessorVersion) -> Result<(String, String, String), String> {
        let catalog = self.libraries()?;
        for entry in catalog["libraries"].as_array().unwrap() {
            for p in entry["processors"].as_array().unwrap() {
                if p["processor_id"] == record.processor_id.as_str()
                    && p["version"] == record.version.as_str()
                {
                    return Ok((
                        entry["id"].as_str().unwrap_or(UNASSIGNED_LIBRARY).into(),
                        p["name"].as_str().unwrap_or("").into(),
                        p["description"].as_str().unwrap_or("").into(),
                    ));
                }
            }
        }
        Ok((
            UNASSIGNED_LIBRARY.into(),
            derived_name(record)?,
            String::new(),
        ))
    }
    /// Register a named definition into a library (default: `unassigned`).
    pub fn register(&self, request: RegisterRequest) -> Result<Value, String> {
        let definition = if request.definition.get("composition").is_some() {
            serde_json::from_value(request.definition)
                .map(ProcessorDefinition::Composition)
                .map_err(|e| e.to_string())?
        } else {
            serde_json::from_value(request.definition)
                .map(ProcessorDefinition::Program)
                .map_err(|e| e.to_string())?
        };
        let library_id = request
            .library_id
            .unwrap_or_else(|| UNASSIGNED_LIBRARY.into());
        validate_name_description(&request.name, &request.description)?;
        if !self.libraries()?["libraries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == library_id.as_str())
        {
            return Err(format!(
                "Unknown library {library_id}; call library_create first"
            ));
        }
        let record = self
            .registry()?
            .create(definition, request.git_provenance)?;
        let association = self.associate(
            &record.processor_id,
            &record.version,
            &library_id,
            &request.name,
            &request.description,
        )?;
        let mut result = serde_json::to_value(&record).map_err(|e| e.to_string())?;
        result["name"] = association["name"].clone();
        result["description"] = association["description"].clone();
        result["library_id"] = json!(library_id);
        Ok(result)
    }
    /// Every version of every processor with its control-plane name and library.
    pub fn definitions(&self) -> Result<Value, String> {
        let registry = self.registry()?;
        let mut cursor: Option<String> = None;
        let mut processors = Vec::new();
        loop {
            let page = registry.list(100, cursor.as_deref(), true)?;
            for summary in &page.processors {
                for record in registry.versions(&summary.processor_id)? {
                    let (library_id, name, description) = self.association(&record)?;
                    processors.push(json!({
                        "processor_id":record.processor_id,"version":record.version,
                        "current":record.version == summary.version,
                        "kind":kind_of(&record.definition),"status":summary.status,
                        "name":name,"description":description,"library_id":library_id,
                        "created_at_unix_ms":record.created_at_unix_ms,
                    }));
                }
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(json!({"processors":processors}))
    }
    /// Import a foreign registry (or one processor with its closure) with
    /// preserved identities, then name and file the admitted definitions.
    pub fn import(&self, request: ImportRequest) -> Result<Value, String> {
        if let Some(library_id) = &request.library_id {
            if !self.libraries()?["libraries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["id"] == library_id.as_str())
            {
                return Err(format!(
                    "Unknown library {library_id}; call library_create first"
                ));
            }
        }
        let report = self.registry()?.import_registry(
            &request.source_registry,
            request.processor_id.as_deref(),
            request.dry_run,
        )?;
        let mut imported = Vec::new();
        for outcome in &report.imported {
            let record = &outcome.record;
            let (existing_library, existing_name, existing_description) =
                self.association(record)?;
            let associated = existing_library != UNASSIGNED_LIBRARY
                || !existing_name.is_empty() && outcome.status == ImportStatus::Present;
            let name = request
                .names
                .get(&record.processor_id)
                .cloned()
                .unwrap_or(existing_name);
            let library_id = request.library_id.clone().unwrap_or(existing_library);
            if !request.dry_run
                && report.errors.is_empty()
                && (!associated
                    || request.library_id.is_some()
                    || request.names.contains_key(&record.processor_id))
            {
                self.associate(
                    &record.processor_id,
                    &record.version,
                    &library_id,
                    &name,
                    &existing_description,
                )?;
            }
            imported.push(json!({"processor_id":record.processor_id,"version":record.version,"kind":kind_of(&record.definition),"status":outcome.status,"current":outcome.current,"name":name,"library_id":library_id}));
        }
        Ok(json!({"imported":imported,"errors":report.errors,"dry_run":request.dry_run}))
    }
    fn scenarios_path(&self, processor_id: &str, version: &str) -> PathBuf {
        self.build_root
            .join("scenarios")
            .join(processor_id)
            .join(format!("{}.json", &version[7..]))
    }
    /// Validate and store the scenarios of one exact definition version.
    pub fn scenarios_set(
        &self,
        processor_id: &str,
        version: &str,
        scenarios: Vec<Scenario>,
    ) -> Result<Value, String> {
        let record = self.registry()?.get(processor_id, Some(version))?;
        validate_scenarios(&public_relations(&record)?, &scenarios)?;
        let directory = self.build_root.join("scenarios").join(processor_id);
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let document = json!({"schema_version":1,"processor":{"processor_id":processor_id,"version":version},"scenarios":scenarios,"updated_at_unix_ms":timestamp()});
        atomic_json(&self.scenarios_path(processor_id, version), &document)?;
        Ok(document)
    }
    pub fn scenarios_get(&self, processor_id: &str, version: &str) -> Result<Value, String> {
        self.registry()?.get(processor_id, Some(version))?;
        let path = self.scenarios_path(processor_id, version);
        if !path.exists() {
            return Ok(
                json!({"schema_version":1,"processor":{"processor_id":processor_id,"version":version},"scenarios":[],"updated_at_unix_ms":null}),
            );
        }
        serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())
    }
    pub fn create_library(&self, definition: LibraryDefinition) -> Result<Value, String> {
        for value in [
            &definition.name,
            &definition.repository,
            &definition.revision,
        ] {
            if value.trim().is_empty() || value.len() > 4096 {
                return Err("Library fields must be nonempty and bounded".into());
            }
        }
        use sha2::{Digest, Sha256};
        let id = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&definition).map_err(|e| e.to_string())?)
        );
        let mut catalog = self.stored_catalog()?;
        let libraries = catalog["libraries"].as_array_mut().unwrap();
        if let Some(existing) = libraries.iter().find(|entry| entry["id"] == id) {
            return Ok(existing.clone());
        }
        let entry = json!({"id":id,"name":definition.name,"repository":definition.repository,"revision":definition.revision,"registered_at_unix_ms":timestamp(),"definitions":"registered_separately","provenance":"operator_supplied","processors":[]});
        libraries.push(entry.clone());
        atomic_json(&self.build_root.join("libraries.json"), &catalog)?;
        Ok(entry)
    }
    /// Existing immutable registry admission. Registration never launches a world.
    pub fn registry(&self) -> Result<ProcessorRegistry, String> {
        ProcessorRegistry::open(self.registry_root.clone())
    }
    pub fn create(&mut self, definition: WorldDefinition) -> Result<String, String> {
        if definition.label.trim().is_empty() || definition.label.len() > 256 {
            return Err("World label must contain 1–256 bytes".into());
        }
        if !matches!(definition.purpose.as_str(), "instance" | "test") {
            return Err("World purpose must be instance or test".into());
        }
        if definition.purpose == "instance" && !definition.scenarios.is_empty() {
            return Err("Scenarios belong to test worlds; create with purpose test".into());
        }
        let registry = self.registry()?;
        registry.ensure_active(&definition.processor.processor_id)?;
        let record = registry.get(
            &definition.processor.processor_id,
            Some(&definition.processor.version),
        )?;
        if definition.purpose == "test" {
            if definition.scenarios.is_empty() {
                return Err("A test world requires at least one scenario".into());
            }
            validate_scenarios(&public_relations(&record)?, &definition.scenarios)?;
        }
        // Exclusive build-directory creation gives every manager/generation its own
        // identity and prevents two owners from sharing native build artifacts.
        let id = crate::bounded::owner_identity();
        std::fs::create_dir(self.build_root.join(&id)).map_err(|e| e.to_string())?;
        self.worlds.insert(
            id.clone(),
            World {
                schema_version: SCHEMA_VERSION,
                definition,
                instance: None,
                state: "created".into(),
                error: None,
                generation: 0,
                metadata: match record.definition {
                    crate::registry::ProcessorDefinition::Program(p) => p.inspection,
                    crate::registry::ProcessorDefinition::Composition(c) => c.inspection,
                },
                telemetry: None,
                tailer: None,
                reader: None,
                capture_generation: None,
                history: vec![],
                pending: None,
                stop_requested: false,
                test: None,
                test_progress: None,
            },
        );
        if let Err(error) = persist(&self.build_root, &id, self.worlds.get_mut(&id).unwrap()) {
            // The reply and the inventory must agree: a world whose record was
            // never written is not inventory, and its directory has no world.json.
            self.worlds.remove(&id);
            if let Err(cleanup) = std::fs::remove_dir_all(self.build_root.join(&id)) {
                return Err(format!(
                    "{error}; world directory {id} could not be removed: {cleanup}"
                ));
            }
            return Err(error);
        }
        Ok(id)
    }
    /// Create an ephemeral test world for stored scenarios and start it. The
    /// reply is the immediate `starting` status; progress is `status.test`.
    pub fn test(&mut self, request: TestRequest) -> Result<Value, String> {
        let record = self
            .registry()?
            .get(&request.processor_id, Some(&request.version))?;
        let stored = self.scenarios_get(&request.processor_id, &request.version)?;
        let stored: Vec<Scenario> =
            serde_json::from_value(stored["scenarios"].clone()).map_err(|e| e.to_string())?;
        if stored.is_empty() {
            return Err(
                "No scenarios are stored for this definition version; call scenarios_set first"
                    .into(),
            );
        }
        let scenarios = match &request.scenarios {
            None => stored,
            Some(names) => {
                let mut selected = Vec::new();
                for name in names {
                    selected.push(
                        stored
                            .iter()
                            .find(|scenario| scenario.name == *name)
                            .cloned()
                            .ok_or_else(|| format!("Unknown scenario {name}"))?,
                    );
                }
                selected
            }
        };
        let (_, name, _) = self.association(&record)?;
        self.ensure_starting_allowed()?;
        let id = self.create(WorldDefinition {
            label: format!("Test · {name}"),
            processor: ProcessorReference {
                processor_id: request.processor_id.clone(),
                version: request.version.clone(),
            },
            purpose: "test".into(),
            scenarios,
        })?;
        match self.start_with(&id, request.keep_world) {
            Ok(status) => Ok(status),
            Err(error) => {
                // A test world that never started is not inventory: the reply
                // and the world store must agree. A world whose start thread is
                // already running is kept and reports through status.
                if self
                    .worlds
                    .get(&id)
                    .is_some_and(|world| world.pending.is_none())
                {
                    self.worlds.remove(&id);
                    if let Err(cleanup) = std::fs::remove_dir_all(self.build_root.join(&id)) {
                        return Err(format!(
                            "{error}; test world {id} could not be removed: {cleanup}"
                        ));
                    }
                }
                Err(error)
            }
        }
    }
    fn ensure_starting_allowed(&self) -> Result<(), String> {
        if self.shutdown.stopped.load(Ordering::SeqCst) {
            return Err(
                "World manager is shutting down; create a new owner before starting".into(),
            );
        }
        Ok(())
    }
    /// Synchronous library convenience. Control-plane clients use start_async.
    pub fn start(&mut self, id: &str) -> Result<Value, String> {
        self.start_async(id)?;
        loop {
            let status = self.status(id)?;
            match status["state"].as_str() {
                Some("starting") => std::thread::sleep(std::time::Duration::from_millis(100)),
                Some("running") => return Ok(status),
                _ => {
                    return Err(status["error"]
                        .as_str()
                        .unwrap_or("World did not start")
                        .into())
                }
            }
        }
    }
    /// Publish starting state before compiling; inventory and stop remain available.
    /// A restarted test world keeps its instance after re-running its scenarios.
    pub fn start_async(&mut self, id: &str) -> Result<Value, String> {
        self.start_with(id, true)
    }
    fn start_with(&mut self, id: &str, keep_world: bool) -> Result<Value, String> {
        let registry = self.registry()?;
        let controls = self.shutdown.controls.clone();
        let mut controls = controls.lock().map_err(|_| "Shutdown lock poisoned")?;
        self.ensure_starting_allowed()?;
        let world = self.worlds.get_mut(id).ok_or("Unknown world")?;
        if world.instance.is_some() || world.pending.is_some() {
            return Err("World already owns an instance; stop it before restarting".into());
        }
        if let Some(mut tailer) = world.tailer.take() {
            tailer.stop();
        }
        world.generation += 1;
        world.stop_requested = false;
        let mut backend = Backend::new(
            self.build_root.join(id).join(world.generation.to_string()),
            self.driver.clone(),
        );
        #[cfg(unix)]
        {
            backend.control = crate::processes::ProcessControl::hosted();
        }
        let log = self
            .build_root
            .join(id)
            .join(world.generation.to_string())
            .join("native-events.jsonl");
        backend.inspection_log = Some(log.clone());
        backend.set_observer(ObserverOptions {
            detail_full: true,
            rotate: true,
        });
        let state = Arc::new(Mutex::new(State::default()));
        let (stop, live) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
        );
        world.telemetry = Some(state.clone());
        world.reader = None;
        world.tailer = Some(Tailer {
            stop: stop.clone(),
            live: live.clone(),
            handle: Some(spawn_tailer(Reader::new(log), state, stop, live)),
        });
        controls.insert(id.into(), backend.control.clone());
        let mut instance =
            ProgramInstance::new(backend, BTreeMap::new(), Some(registry), Some(id.into()));
        world.state = "starting".into();
        world.error = None;
        let scenarios = if world.definition.purpose == "test" {
            world.definition.scenarios.clone()
        } else {
            Vec::new()
        };
        let progress = if scenarios.is_empty() {
            world.test = None;
            world.test_progress = None;
            None
        } else {
            let progress = Arc::new(Mutex::new(
                json!({"phase":"building","scenario_index":0,"results":[],"passed":null,"error":null}),
            ));
            world.test = Some(progress.lock().unwrap().clone());
            world.test_progress = Some(progress.clone());
            Some(progress)
        };
        if let Err(error) = persist(&self.build_root, id, world) {
            // No start thread exists yet: stop the tailer spawned above and
            // forget the process control so neither outlives this attempt.
            if let Some(mut tailer) = world.tailer.take() {
                tailer.stop();
            }
            controls.remove(id);
            world.state = "failed".into();
            world.error = Some(format!("Cannot persist starting state: {error}"));
            return Err(error);
        }
        let pin = world.definition.processor.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        world.pending = Some(receiver);
        std::thread::spawn(move || {
            let installed = instance.execute(
                "processor_install",
                &json!({"processor_id":pin.processor_id,"version":pin.version}),
            );
            let result: StartOutcome = match (installed, progress) {
                (Err(error), Some(progress)) => {
                    let mut status = progress.lock().unwrap();
                    status["phase"] = json!("failed");
                    status["passed"] = json!(false);
                    status["error"] = json!(error);
                    Err(error)
                }
                (Err(error), None) => Err(error),
                (Ok(_), None) => Ok(Some(instance)),
                (Ok(_), Some(progress)) => {
                    run_scenarios(&mut instance, &scenarios, &progress);
                    if keep_world {
                        Ok(Some(instance))
                    } else {
                        drop(instance);
                        Ok(None)
                    }
                }
            };
            let _ = sender.send(result);
        });
        drop(controls);
        self.status(id)
    }
    pub fn stop(&mut self, id: &str) -> Result<Value, String> {
        let world = self.worlds.get_mut(id).ok_or("Unknown world")?;
        if let Some(instance) = world.instance.take() {
            #[cfg(unix)]
            instance.backend.control.stop();
            drop(instance); // Existing Backend/Runtime drop kills and reaps the native child.
        }
        if let Some(mut tailer) = world.tailer.take() {
            tailer.stop();
        }
        world.stop_requested = true;
        if let Some(control) = self
            .shutdown
            .controls
            .lock()
            .map_err(|_| "Shutdown lock poisoned")?
            .get(id)
        {
            #[cfg(unix)]
            control.stop();
        }
        world.state = if world.pending.is_some() {
            "stopping"
        } else {
            "stopped"
        }
        .into();
        persist(&self.build_root, id, world)?;
        self.status(id)
    }
    pub fn execute(&mut self, id: &str, operation: &str, args: &Value) -> Result<Value, String> {
        self.status(id)?;
        if operation.starts_with("processor_")
            || matches!(
                operation,
                "lemmalog_install_rules" | "install_agent_program"
            )
        {
            return Err(
                "Manage definitions through the registry; a world is pinned at creation".into(),
            );
        }
        self.worlds
            .get_mut(id)
            .ok_or("Unknown world")?
            .instance
            .as_mut()
            .ok_or("World is not running")?
            .execute(operation, args)
    }
    pub fn status(&mut self, id: &str) -> Result<Value, String> {
        self.status_with(id, false)
    }
    fn status_with(&mut self, id: &str, summary: bool) -> Result<Value, String> {
        let world = self.worlds.get_mut(id).ok_or("Unknown world")?;
        let completion = world.pending.as_ref().map(|receiver| receiver.try_recv());
        match completion {
            Some(Ok(result)) => {
                world.pending = None;
                if world.stop_requested {
                    drop(result);
                    world.state = "stopped".into();
                } else {
                    match result {
                        Ok(Some(instance)) => {
                            world.instance = Some(instance);
                            world.state = "running".into();
                            world.error = None;
                        }
                        Ok(None) => {
                            world.state = "stopped".into();
                            world.error = None;
                        }
                        Err(error) => {
                            world.state = "failed".into();
                            world.error = Some(error);
                        }
                    }
                }
                if world.instance.is_none() {
                    if let Some(tailer) = &world.tailer {
                        tailer.live.store(false, Ordering::SeqCst);
                    }
                }
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                world.pending = None;
                world.state = "failed".into();
                world.error = Some("World start worker exited without a result".into());
                if let Some(tailer) = &world.tailer {
                    tailer.live.store(false, Ordering::SeqCst);
                }
            }
            _ => (),
        }
        if let Some(progress) = &world.test_progress {
            let observed = progress
                .lock()
                .map_err(|_| "Test progress lock poisoned")?
                .clone();
            if world.test.as_ref() != Some(&observed) {
                world.test = Some(observed);
                persist(&self.build_root, id, world)?;
            }
        }
        let mut pid = None;
        let mut revision = Value::Null;
        let mut instance_info = Value::Null;
        let mut health_failed = false;
        if let Some(instance) = &mut world.instance {
            if instance.backend.observed_health() == "failed" {
                world.state = "failed".into();
                world.error.get_or_insert_with(|| {
                    "Native execution process exited unexpectedly; start begins a new generation"
                        .into()
                });
                health_failed = true;
            } else {
                pid = instance.backend.runtime_pid();
                // The committed revision is an in-memory counter: no native exchange,
                // so summary inventories can carry it for every live world.
                revision = json!(instance.backend.revision());
                if !summary {
                    instance_info = instance.execute("instance_info", &json!({}))?;
                }
            }
        }
        if health_failed {
            // The child is gone: dropping the instance reaps it and its process
            // group, so `start` (valid for `failed`) opens a fresh generation.
            if let Some(instance) = world.instance.take() {
                #[cfg(unix)]
                instance.backend.control.stop();
                drop(instance);
            }
            if let Some(mut tailer) = world.tailer.take() {
                tailer.stop();
            }
        }
        persist_if_changed(&self.build_root, id, world)?;
        let generation_dir = self.build_root.join(id).join(world.generation.to_string());
        let build = if world.state == "starting"
            || (world.state == "failed" && world.instance.is_none() && world.generation > 0)
        {
            let log = generation_dir.join("build-1").join("build.log");
            json!({"generation":world.generation,"log_tail":tail_lines(&log, BUILD_LOG_TAIL_LINES),"instrumented":instrumented(&generation_dir)})
        } else {
            Value::Null
        };
        let started_at = world
            .history
            .iter()
            .rev()
            .find(|entry| {
                entry["state"] == "starting" && entry["generation"] == json!(world.generation)
            })
            .map(|entry| entry["at_unix_ms"].clone())
            .unwrap_or(Value::Null);
        let mut status = json!({"schema_version":SCHEMA_VERSION,"id":id,"definition":world.definition,
            "state":world.state,"generation":world.generation,"error":world.error,"history":world.history,
            "owner":{"kind":"embedded","pid":std::process::id(),"memory_attribution":"shared_not_attributable"},
            "resources":sample_process(pid),"started_at_unix_ms":started_at,"build":build,"revision":revision,
            "persistence":{"status":"not_configured","reason":"world checkpoints are not wired"},
            "test":world.test});
        if !summary {
            let processes = self.shutdown.controls.lock().map_err(|_|"Shutdown lock poisoned")?.get(id).map(|control|control.tracked_pids()).unwrap_or_default().into_iter().map(|process|json!({"role":if Some(process)==pid {"native"} else {"compiler_or_startup"},"sample":sample_process(Some(process)),"descendants_included":false})).collect::<Vec<_>>();
            status["managed_processes"] = json!(processes);
            status["instance"] = instance_info;
            // A recovered world reads its last capture once; nothing tails it.
            if let (Some(reader), Some(state)) = (world.reader.as_mut(), &world.telemetry) {
                reader.drain(state);
            }
            world.reader = None;
            let (inspection, schedule_seen) = match &world.telemetry {
                Some(state) => {
                    let state = state.lock().map_err(|_| "Capture state lock poisoned")?;
                    (
                        state.snapshot(world.metadata.as_ref()),
                        state.schedule_seen(),
                    )
                }
                None => (json!({"state":"missing","metadata":world.metadata}), false),
            };
            if world.capture_generation != Some(world.generation)
                && inspection["state"] == "available"
                && inspection["unresolved_channels"] == 0
                && schedule_seen
            {
                retain_capture(&self.build_root, id, world, &inspection)?;
                world.capture_generation = Some(world.generation);
                persist(&self.build_root, id, world)?;
            }
            status["inspection"] = inspection;
        }
        Ok(status)
    }
    /// The retained compiled topology of one definition version, or `{state: missing}`.
    pub fn capture_get(&self, processor_id: &str, version: &str) -> Result<Value, String> {
        self.registry()?.get(processor_id, Some(version))?;
        let path = capture_path(&self.build_root, processor_id, version);
        if !path.exists() {
            return Ok(json!({"state":"missing"}));
        }
        serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())
    }
    pub fn inventory(&mut self, query: &InventoryQuery) -> Result<Value, String> {
        let ids: Vec<_> = self
            .worlds
            .iter()
            .filter(|(_, world)| {
                query
                    .processor_id
                    .as_ref()
                    .is_none_or(|p| *p == world.definition.processor.processor_id)
                    && query
                        .version
                        .as_ref()
                        .is_none_or(|v| *v == world.definition.processor.version)
            })
            .map(|(id, _)| id.clone())
            .collect();
        let worlds: Result<Vec<_>, _> = ids
            .iter()
            .map(|id| self.status_with(id, query.summary))
            .collect();
        Ok(
            json!({"schema_version":SCHEMA_VERSION,"scope":"owned_by_this_manager_with_history","summary":query.summary,"worlds":worlds?}),
        )
    }
}
/// Default definition name from its public interface: `outputs ← inputs`, or
/// `Composition of <aliases>` for compositions.
pub fn derived_name(record: &ProcessorVersion) -> Result<String, String> {
    if let ProcessorDefinition::Composition(definition) = &record.definition {
        let aliases: Vec<&str> = definition
            .composition
            .nodes
            .keys()
            .map(String::as_str)
            .collect();
        return Ok(format!("Composition of {}", aliases.join(", ")));
    }
    let relations = public_relations(record)?;
    let names = |input: bool| -> String {
        relations
            .iter()
            .filter(|relation| relation.input == input)
            .map(|relation| relation.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (inputs, outputs) = (names(true), names(false));
    Ok(match (inputs.is_empty(), outputs.is_empty()) {
        (false, false) => format!("{outputs} ← {inputs}"),
        (true, false) => outputs,
        (false, true) => inputs,
        (true, true) => record.processor_id.clone(),
    })
}
fn typed(value: &Value, field: &str) -> bool {
    match field {
        "int" => value.as_i64().is_some(),
        "string" => value.is_string(),
        _ => false,
    }
}
fn row_matches(row: &[Value], fields: &[String], what: &str) -> Result<(), String> {
    if row.len() != fields.len() {
        return Err(format!(
            "{what} has {} values; relation arity is {}",
            row.len(),
            fields.len()
        ));
    }
    for (index, (value, field)) in row.iter().zip(fields).enumerate() {
        if !typed(value, field) {
            return Err(format!("{what} value {index} must be {field}"));
        }
    }
    Ok(())
}
/// Scenario predicates must be public inputs (changes) or public relations
/// (expect); rows must match arity and types; expected rows are sets.
pub fn validate_scenarios(
    relations: &[PublicRelation],
    scenarios: &[Scenario],
) -> Result<(), String> {
    let mut names = BTreeSet::new();
    for scenario in scenarios {
        if scenario.name.trim().is_empty() || scenario.name.len() > 256 {
            return Err("Scenario name must contain 1–256 bytes".into());
        }
        if !names.insert(scenario.name.clone()) {
            return Err(format!("Duplicate scenario name {}", scenario.name));
        }
        for (index, change) in scenario.changes.iter().enumerate() {
            if !matches!(change.op.as_str(), "insert" | "delete") {
                return Err(format!(
                    "Scenario {}: change {index} op must be insert or delete",
                    scenario.name
                ));
            }
            let relation = relations
                .iter()
                .find(|relation| relation.name == change.predicate && relation.input)
                .ok_or_else(|| {
                    format!(
                        "Scenario {}: change {index} predicate {} is not a public input",
                        scenario.name, change.predicate
                    )
                })?;
            row_matches(
                &change.values,
                &relation.fields,
                &format!("Scenario {}: change {index}", scenario.name),
            )?;
        }
        for (name, rows) in &scenario.expect {
            let relation = relations
                .iter()
                .find(|relation| relation.name == *name)
                .ok_or_else(|| {
                    format!(
                        "Scenario {}: expected relation {name} is not public",
                        scenario.name
                    )
                })?;
            let mut seen = BTreeSet::new();
            for (index, row) in rows.iter().enumerate() {
                row_matches(
                    row,
                    &relation.fields,
                    &format!("Scenario {}: expected {name} row {index}", scenario.name),
                )?;
                if !seen.insert(serde_json::to_string(row).map_err(|e| e.to_string())?) {
                    return Err(format!("Scenario {}: expected {name} row {index} is a duplicate; relations are sets", scenario.name));
                }
            }
        }
    }
    Ok(())
}
/// Read one whole public relation through paged `query_rows`.
fn read_relation(
    instance: &mut ProgramInstance,
    predicate: &str,
) -> Result<(u64, Vec<Vec<Value>>), String> {
    let mut rows = Vec::new();
    let mut continuation = Value::Null;
    let mut revision = 0;
    loop {
        let page = instance.execute("query_rows", &json!({"predicate":predicate,"max_rows":crate::MAX_QUERY_ROWS,"continuation":continuation}))?;
        revision = page["revision"].as_u64().unwrap_or(revision);
        rows.extend(
            serde_json::from_value::<Vec<Vec<Value>>>(page["rows"].clone())
                .map_err(|e| e.to_string())?,
        );
        if page["complete"] == json!(true) {
            return Ok((revision, rows));
        }
        continuation = page["continuation"].clone();
    }
}
/// Apply every scenario cumulatively on the freshly installed instance and
/// compare each expected relation as a set. Progress is published per scenario.
fn run_scenarios(
    instance: &mut ProgramInstance,
    scenarios: &[Scenario],
    progress: &Arc<Mutex<Value>>,
) {
    let publish = |phase: &str, index: usize, results: &[Value], passed: Value| {
        if let Ok(mut status) = progress.lock() {
            *status = json!({"phase":phase,"scenario_index":index,"results":results,"passed":passed,"error":null});
        }
    };
    let mut results: Vec<Value> = Vec::new();
    publish("applying", 0, &results, Value::Null);
    for (index, scenario) in scenarios.iter().enumerate() {
        let mut result = json!({"name":scenario.name,"passed":false,"revision":null,"expected":{},"observed":{},"missing":{},"unexpected":{},"error":null});
        let outcome = (|| -> Result<(), String> {
            let applied =
                instance.execute("apply_changes", &json!({"changes":scenario.changes}))?;
            result["revision"] = applied["revision"].clone();
            let mut passed = true;
            for (name, expected) in &scenario.expect {
                let (revision, observed) = read_relation(instance, name)?;
                result["revision"] = json!(revision);
                let key = |row: &Vec<Value>| serde_json::to_string(row).unwrap_or_default();
                let expected_keys: BTreeSet<String> = expected.iter().map(key).collect();
                let observed_keys: BTreeSet<String> = observed.iter().map(key).collect();
                let missing: Vec<&Vec<Value>> = expected
                    .iter()
                    .filter(|row| !observed_keys.contains(&key(row)))
                    .collect();
                let unexpected: Vec<&Vec<Value>> = observed
                    .iter()
                    .filter(|row| !expected_keys.contains(&key(row)))
                    .collect();
                passed &= missing.is_empty() && unexpected.is_empty();
                result["expected"][name] = json!(expected);
                result["observed"][name] = json!(observed);
                result["missing"][name] = json!(missing);
                result["unexpected"][name] = json!(unexpected);
            }
            result["passed"] = json!(passed);
            Ok(())
        })();
        if let Err(error) = outcome {
            result["error"] = json!(error);
            result["passed"] = json!(false);
        }
        results.push(result);
        publish("applying", index + 1, &results, Value::Null);
        if instance.backend.health() == "failed" {
            for remaining in &scenarios[index + 1..] {
                results.push(json!({"name":remaining.name,"passed":false,"revision":null,"expected":{},"observed":{},"missing":{},"unexpected":{},"error":"Instance runtime failed before this scenario ran"}));
            }
            break;
        }
    }
    let passed = results.iter().all(|result| result["passed"] == json!(true));
    publish("done", results.len(), &results, json!(passed));
}
fn capture_path(build_root: &Path, processor_id: &str, version: &str) -> PathBuf {
    build_root
        .join("captures")
        .join(processor_id)
        .join(format!("{}.json", &version[7..]))
}
/// Persist the compiled topology of a definition version (A8): the native
/// graph and resolved metadata, without activity. Later generations of any
/// world pinned to the same version replace it.
fn retain_capture(
    build_root: &Path,
    id: &str,
    world: &World,
    inspection: &Value,
) -> Result<(), String> {
    let pin = &world.definition.processor;
    std::fs::create_dir_all(build_root.join("captures").join(&pin.processor_id))
        .map_err(|e| e.to_string())?;
    let document = json!({"schema_version":1,"processor":pin,"world_id":id,"generation":world.generation,
        "captured_at_unix_ms":timestamp(),"unresolved_channels":inspection["unresolved_channels"],
        "graph":inspection["graph"],"metadata":inspection["metadata"],"mapping_error":inspection["mapping_error"]});
    atomic_json(
        &capture_path(build_root, &pin.processor_id, &pin.version),
        &document,
    )
}
/// Last `count` lines of a build log, or null until the driver creates it.
/// Only the final 64 KiB are read so a verbose compiler cannot stall status.
fn tail_lines(path: &Path, count: usize) -> Value {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return Value::Null;
    };
    let length = file.metadata().map(|m| m.len()).unwrap_or(0);
    let window = 64 * 1024;
    let start = length.saturating_sub(window);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Value::Null;
    }
    let mut bytes = Vec::new();
    if file.take(window).read_to_end(&mut bytes).is_err() {
        return Value::Null;
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(count);
    json!(lines[skip..].join("\n"))
}
/// The build driver's `install-observer.py` records its completed patch set in
/// `program_ddlog/observer-install.json`; without that marker the native child
/// carries no observer hook or star phase regions.
fn instrumented(generation_dir: &Path) -> bool {
    let path = generation_dir
        .join("build-1")
        .join("program_ddlog")
        .join("observer-install.json");
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|marker| marker["observer_hook"] == json!(true))
}
/// Independent cancellation handle used by an embedding owner on shutdown.
/// Cancels managed compiler/native process groups even during a blocking install.
#[derive(Clone, Default)]
pub struct WorldShutdown {
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    controls: std::sync::Arc<std::sync::Mutex<BTreeMap<String, crate::processes::ProcessControl>>>,
}
impl WorldShutdown {
    pub fn stop_all(&self) {
        if let Ok(controls) = self.controls.lock() {
            self.stopped
                .store(true, std::sync::atomic::Ordering::SeqCst);
            for control in controls.values() {
                #[cfg(unix)]
                control.stop();
            }
        }
    }
}
impl Drop for WorldManager {
    fn drop(&mut self) {
        self.shutdown.stop_all();
        for (id, world) in self.worlds.iter_mut() {
            if let Some(instance) = world.instance.take() {
                #[cfg(unix)]
                instance.backend.control.stop();
                drop(instance);
                world.state = "stopped".into();
                let _ = persist(&self.build_root, id, world);
            }
            if let Some(mut tailer) = world.tailer.take() {
                tailer.stop();
            }
        }
    }
}
fn timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
/// Measurements cover the native child only, excluding compiler descendants and
/// the embedding owner. CPU is ps's lifetime-average percentage, not an interval.
pub fn sample_process(pid: Option<u32>) -> Value {
    let mut result = json!({"sampled_at_unix_ms":timestamp(),"state":"missing","pid":pid,
        "scope":"process","cpu_percent":null,"cpu_basis":if cfg!(target_os="macos") {"ps_decaying_average"} else {"ps_lifetime_average"},
        "resident_bytes":null,"max_age_ms":5000});
    let Some(pid) = pid else {
        return result;
    };
    #[cfg(unix)]
    {
        let output = std::process::Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "%cpu=", "-o", "rss="])
            .env("LC_ALL", "C")
            .output();
        if let Ok(output) = output {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                let mut fields = text.split_whitespace();
                if let (Some(cpu), Some(rss)) = (
                    fields.next().and_then(|s| s.parse::<f64>().ok()),
                    fields.next().and_then(|s| s.parse::<u64>().ok()),
                ) {
                    if cpu.is_finite() && cpu >= 0.0 {
                        result["state"] = json!("available");
                        result["cpu_percent"] = json!(cpu);
                        result["resident_bytes"] = json!(rss.saturating_mul(1024));
                    }
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        result["state"] = json!("unsupported");
    }
    result
}

/// Checked before any registry or catalog write so a refused `register` leaves
/// nothing behind.
fn validate_name_description(name: &str, description: &str) -> Result<(), String> {
    if name.trim().is_empty() || name.len() > 256 || description.len() > 4096 {
        return Err("Definition name must contain 1–256 bytes; description at most 4096".into());
    }
    Ok(())
}
fn lock_owner(root: &std::path::Path) -> Result<std::fs::File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(root.join(".owner.lock"))
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // A lock released by a dropped owner can still be held for a moment by a
        // child spawned meanwhile (its copied descriptor table keeps the open file
        // description until exec closes it), so a fresh owner retries briefly
        // before reporting a genuine owner.
        let mut attempt = 0;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EWOULDBLOCK) | Some(libc::EINTR) => (),
                _ => return Err(format!("Cannot lock world store: {error}")),
            }
            attempt += 1;
            if attempt >= OWNER_LOCK_ATTEMPTS {
                return Err("World store already has an owner; attach to its control plane".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(OWNER_LOCK_RETRY_MS));
        }
    }
    Ok(file)
}
fn persist_if_changed(root: &std::path::Path, id: &str, world: &mut World) -> Result<(), String> {
    if world.history.last().is_none_or(|last| {
        last["state"] != world.state
            || last["generation"] != world.generation
            || last["error"] != json!(world.error)
    }) {
        persist(root, id, world)?;
    }
    Ok(())
}
fn persist(root: &std::path::Path, id: &str, world: &mut World) -> Result<(), String> {
    let mut history = world.history.clone();
    if history.last().is_none_or(|last| {
        last["state"] != world.state
            || last["generation"] != world.generation
            || last["error"] != json!(world.error)
    }) {
        history.push(json!({"at_unix_ms":timestamp(),"state":world.state,"generation":world.generation,"error":world.error}));
    }
    let path = root.join(id).join("world.json");
    let temp = path.with_extension("json.tmp");
    let mut record = serde_json::to_value(&*world).map_err(|e| e.to_string())?;
    record["history"] = json!(history);
    let bytes = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temp).map_err(|e| e.to_string())?;
    use std::io::Write;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    std::fs::rename(&temp, &path).map_err(|e| e.to_string())?;
    std::fs::File::open(root.join(id))
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    world.history = history;
    Ok(())
}

fn atomic_json(path: &std::path::Path, value: &Value) -> Result<(), String> {
    let temp = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temp).map_err(|e| e.to_string())?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    std::fs::rename(&temp, path).map_err(|e| e.to_string())?;
    std::fs::File::open(path.parent().ok_or("Missing catalog parent")?)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())
}
