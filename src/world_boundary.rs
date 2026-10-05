//! Opt-in external publication on the existing hosted admission owner.
//! Native work moves into one owned per-world job; the durable admission record
//! remains the barrier. Neither native death nor a missing response clears it.
use super::{admission, persistence, workers, AdmissionQuery, AdmitInputs, World, WorldManager};
use crate::ProgramInstance;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;

type Result<T> = std::result::Result<T, String>;
const MAX_FROZEN_BYTES: usize = 64 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalPublicationPolicy {
    pub namespace: String,
    pub outputs: Vec<String>,
    pub max_rows: usize,
    pub max_bytes: usize,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationBinding {
    pub context: Value,
    pub parent_receipt_sha256: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryAdmission {
    pub admission: AdmitInputs,
    pub binding: PublicationBinding,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryKey {
    pub world_id: String,
    pub generation: u64,
    pub admission_key: String,
    pub request_sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenBlob {
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenOutput {
    pub name: String,
    pub fields: Vec<String>,
    pub rows: usize,
    pub blob: FrozenBlob,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenManifest {
    pub schema_version: u32,
    pub key: BoundaryKey,
    pub policy: ExternalPublicationPolicy,
    pub binding: PublicationBinding,
    pub revision: u64,
    /// Exact managed checkpoint identity, including origin activation provenance.
    pub checkpoint_receipt: Value,
    pub checkpoint: FrozenBlob,
    pub outputs: Vec<FrozenOutput>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalReceipt {
    pub frozen_manifest_sha256: String,
    pub receipt_sha256: String,
    pub receipt: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenBlobRead {
    pub key: BoundaryKey,
    pub blob_sha256: String,
    pub offset: u64,
    pub max_bytes: usize,
}
#[derive(Serialize)]
pub struct FrozenBlobPage {
    pub bytes: Vec<u8>,
    pub next_offset: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundCheckpointRestore {
    pub target_world_id: String,
    pub expected_generation: u64,
    pub manifest: FrozenManifest,
    pub checkpoint_bytes: Vec<u8>,
    pub published: ExternalReceipt,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BoundaryRecord {
    policy: ExternalPublicationPolicy,
    binding: PublicationBinding,
    checkpoint_id: String,
    pub(super) manifest: Option<FrozenManifest>,
    pub(super) external_receipt: Option<ExternalReceipt>,
}
impl BoundaryRecord {
    pub(super) fn key(&self, id: &str, record: &admission::Record) -> BoundaryKey {
        BoundaryKey {
            world_id: id.into(),
            generation: record.generation,
            admission_key: record.admission_key.clone(),
            request_sha256: record.request_sha256.clone(),
        }
    }
}
struct Completion {
    instance: ProgramInstance,
    revision: Option<u64>,
    manifest: Option<FrozenManifest>,
    error: Option<String>,
}
pub(super) struct Job {
    key: BoundaryKey,
    receiver: Receiver<Completion>,
    thread: Option<JoinHandle<()>>,
}
impl Job {
    pub(super) fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        // The owner cancels the existing ProcessControl before teardown joins.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn encoded(value: &impl Serialize) -> Result<Vec<u8>> {
    // Match admission's canonical digest, independent of struct field order.
    serde_json::to_vec(&serde_json::to_value(value).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}
// Keep caller JSON well below the parser limit after both durable envelope
// layers are added. Check depth before serialization, including local Rust Values.
fn validate_json(value: &Value, max_bytes: usize) -> Result<()> {
    fn depth(value: &Value, level: usize) -> Result<()> {
        if level > 32 {
            return Err("Publication JSON exceeds depth 32".into());
        }
        match value {
            Value::Array(values) => {
                for value in values {
                    depth(value, level + 1)?;
                }
            }
            Value::Object(values) => {
                for value in values.values() {
                    depth(value, level + 1)?;
                }
            }
            _ => (),
        }
        Ok(())
    }
    depth(value, 0)?;
    if encoded(value)?.len() > max_bytes {
        return Err("Publication JSON exceeds byte limit".into());
    }
    Ok(())
}
pub(super) fn validate_binding(binding: &PublicationBinding) -> Result<()> {
    validate_json(&binding.context, 64 * 1024)?;
    if binding
        .parent_receipt_sha256
        .as_deref()
        .is_some_and(|h| !valid_hash(h))
    {
        return Err("Invalid external parent digest".into());
    }
    Ok(())
}
fn validate_policy(policy: &ExternalPublicationPolicy) -> Result<()> {
    workers::token(&policy.namespace)?;
    if policy.outputs.is_empty()
        || policy.outputs.len() > 64
        || policy.outputs.iter().collect::<BTreeSet<_>>().len() != policy.outputs.len()
        || policy.max_rows == 0
        || policy.max_rows > 1_000_000
        || policy.max_bytes == 0
        || policy.max_bytes > MAX_FROZEN_BYTES
    {
        return Err("External publication policy exceeds output/row/byte limits".into());
    }
    Ok(())
}
pub(super) fn validate_definition(
    definition: &super::WorldDefinition,
    registry: &crate::registry::ProcessorRegistry,
    record: &crate::registry::ProcessorVersion,
) -> Result<()> {
    let Some(policy) = &definition.external_publication else {
        return Ok(());
    };
    validate_policy(policy)?;
    if definition.purpose != "instance" || !definition.scenarios.is_empty() {
        return Err("External publication requires a pure instance without scenarios".into());
    }
    crate::instance::validate_checkpoint_program(registry, record)?;
    let relations = crate::instance::public_relations(record)?;
    if policy
        .outputs
        .iter()
        .any(|name| !relations.iter().any(|r| &r.name == name && !r.input))
    {
        return Err("External policy selects an unknown/private/input relation".into());
    }
    Ok(())
}
pub(super) fn require_ordinary(world: &World) -> Result<()> {
    if world.definition.external_publication.is_some() {
        return Err("External publication world requires boundary admission/restore".into());
    }
    Ok(())
}
pub(super) fn guard_execute(world: &World, operation: &str) -> Result<()> {
    if world.definition.external_publication.is_some()
        && !matches!(
            operation,
            "instance_info"
                | "relations"
                | "query_rows"
                | "program_source"
                | "lemmalog_query"
                | "lemmalog_why"
        )
    {
        return Err("External publication world rejects unrestricted execution".into());
    }
    Ok(())
}
pub(super) fn guard_start(world: &World, restore: Option<&persistence::Restore>) -> Result<()> {
    if world.boundary_job.is_some() || world.admission.external_pending.is_some() {
        return Err("Unresolved external publication blocks activation".into());
    }
    if world.definition.external_publication.is_some() {
        match restore {
            None if world.admission.records.is_empty()
                && world.admission.external_head.is_none() => {}
            Some(r) if r.receipt.boundary.is_some() && world.admission.external_head.is_some() => {}
            _ => return Err("External publication world requires explicit bound restore".into()),
        }
    }
    Ok(())
}
fn validate_key(key: &BoundaryKey) -> Result<()> {
    persistence::component(&key.world_id)?;
    workers::token(&key.admission_key)?;
    if key.generation == 0 || !valid_hash(&key.request_sha256) {
        return Err("Invalid boundary identity".into());
    }
    Ok(())
}
fn directory(root: &Path, key: &BoundaryKey) -> Result<PathBuf> {
    validate_key(key)?;
    Ok(root
        .join(&key.world_id)
        .join("boundaries")
        .join(format!("{}-{}", key.generation, key.request_sha256)))
}
fn prepare_directory(root: &Path, key: &BoundaryKey) -> Result<PathBuf> {
    let world = root.join(&key.world_id);
    persistence::checkpoint_root(root, &key.world_id)?;
    let parent = world.join("boundaries");
    persistence::private_dir(&parent)?;
    let dir = directory(root, key)?;
    persistence::private_dir(&dir)?;
    for path in [&world, &parent, &dir] {
        File::open(path)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
    }
    Ok(dir)
}
fn immutable(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.try_exists().map_err(|e| e.to_string())? {
        if persistence::read_bounded(path, bytes.len() as u64)? != bytes {
            return Err("Frozen object identity conflict".into());
        }
        File::open(path)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        File::open(path.parent().unwrap())
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(())
    } else {
        persistence::atomic_bytes(path, bytes)
    }
}
fn write_blob(dir: &Path, bytes: &[u8]) -> Result<FrozenBlob> {
    let blob = FrozenBlob {
        sha256: hash(bytes),
        bytes: bytes.len() as u64,
    };
    immutable(&dir.join(&blob.sha256), bytes)?;
    Ok(blob)
}
fn load_blob(dir: &Path, blob: &FrozenBlob) -> Result<Vec<u8>> {
    if !valid_hash(&blob.sha256)
        || blob.bytes > crate::checkpoint::MAX_BYTES.max(MAX_FROZEN_BYTES as u64)
    {
        return Err("Invalid frozen blob descriptor".into());
    }
    let bytes = persistence::read_bounded(&dir.join(&blob.sha256), blob.bytes)?;
    if bytes.len() as u64 != blob.bytes || hash(&bytes) != blob.sha256 {
        return Err("Frozen blob digest mismatch".into());
    }
    Ok(bytes)
}
fn checkpoint_binding(key: &BoundaryKey, boundary: &BoundaryRecord) -> Value {
    json!({"key":key,"policy":boundary.policy,"binding":boundary.binding})
}
pub(super) fn validate_manifest(manifest: &FrozenManifest) -> Result<persistence::Receipt> {
    validate_key(&manifest.key)?;
    validate_policy(&manifest.policy)?;
    validate_binding(&manifest.binding)?;
    let receipt: persistence::Receipt =
        serde_json::from_value(manifest.checkpoint_receipt.clone()).map_err(|e| e.to_string())?;
    receipt.validate()?;
    if manifest.schema_version != 1
        || manifest.revision != receipt.origin.revision
        || receipt.origin.world_id != manifest.key.world_id
        || receipt.origin.generation != manifest.key.generation
        || receipt.boundary
            != Some(json!({"key":manifest.key,"policy":manifest.policy,"binding":manifest.binding}))
        || receipt.format != "json"
        || receipt.storage.is_some()
        || manifest.outputs.iter().map(|r| &r.name).collect::<Vec<_>>()
            != manifest.policy.outputs.iter().collect::<Vec<_>>()
    {
        return Err("Frozen checkpoint/manifest identity mismatch".into());
    }
    let mut bytes = 0u64;
    for output in &manifest.outputs {
        let relation = receipt
            .program
            .public_relations
            .iter()
            .find(|r| r.name == output.name && !r.input)
            .ok_or("Frozen output is not public")?;
        if output.fields != relation.fields
            || output.rows > manifest.policy.max_rows
            || !valid_hash(&output.blob.sha256)
        {
            return Err("Frozen output schema/row mismatch".into());
        }
        bytes = bytes
            .checked_add(output.blob.bytes)
            .ok_or("Frozen size overflow")?;
    }
    if bytes > manifest.policy.max_bytes as u64
        || !valid_hash(&manifest.checkpoint.sha256)
        || manifest.checkpoint.bytes > crate::checkpoint::MAX_BYTES
    {
        return Err("Frozen manifest exceeds byte bound".into());
    }
    Ok(receipt)
}
fn read_manifest(root: &Path, key: &BoundaryKey) -> Result<Option<FrozenManifest>> {
    let dir = directory(root, key)?;
    let path = dir.join("manifest.json");
    if !path.try_exists().map_err(|e| e.to_string())? {
        return Ok(None);
    }
    persistence::existing_dir(&dir)?;
    let manifest: FrozenManifest =
        serde_json::from_slice(&persistence::read_bounded(&path, MAX_MANIFEST_BYTES)?)
            .map_err(|e| e.to_string())?;
    if &manifest.key != key {
        return Err("Frozen manifest key mismatch".into());
    }
    let receipt = validate_manifest(&manifest)?;
    persistence::validate_snapshot(&receipt, &load_blob(&dir, &manifest.checkpoint)?)?;
    for output in &manifest.outputs {
        load_blob(&dir, &output.blob)?;
    }
    Ok(Some(manifest))
}
fn freeze(
    root: &Path,
    key: &BoundaryKey,
    boundary: &BoundaryRecord,
    instance: &mut ProgramInstance,
) -> Result<FrozenManifest> {
    if let Some(existing) = read_manifest(root, key)? {
        return Ok(existing);
    }
    let dir = prepare_directory(root, key)?;
    let revision = instance.backend.revision();
    let (receipt, bytes) = persistence::freeze_instance(
        instance,
        &key.world_id,
        key.generation,
        "json",
        boundary.checkpoint_id.clone(),
        Some(checkpoint_binding(key, boundary)),
        &Default::default(),
    )?;
    let checkpoint = write_blob(&dir, &bytes)?;
    let mut outputs = Vec::new();
    let mut total_bytes = 0usize;
    for name in &boundary.policy.outputs {
        let relation = receipt
            .program
            .public_relations
            .iter()
            .find(|r| &r.name == name && !r.input)
            .ok_or("Unknown public output")?;
        let mut rows: Vec<Vec<Value>> = Vec::new();
        let mut continuation = Value::Null;
        loop {
            let page = instance.execute("query_rows",&json!({"predicate":name,"max_rows":1000,"max_bytes":crate::MAX_QUERY_BYTES,"continuation":continuation}))?;
            if page["revision"] != revision || page["fields"] != json!(relation.fields) {
                return Err("Output revision/schema changed during freeze".into());
            }
            let chunk: Vec<Vec<Value>> =
                serde_json::from_value(page["rows"].clone()).map_err(|e| e.to_string())?;
            let chunk_bytes = if rows.is_empty() {
                encoded(&chunk)?.len()
            } else if chunk.is_empty() {
                0
            } else {
                encoded(&chunk)?.len() - 1
            };
            total_bytes = total_bytes
                .checked_add(chunk_bytes)
                .ok_or("Frozen size overflow")?;
            if total_bytes > boundary.policy.max_bytes
                || rows.len().saturating_add(chunk.len()) > boundary.policy.max_rows
                || page["total"]
                    .as_u64()
                    .is_none_or(|n| n > boundary.policy.max_rows as u64)
            {
                return Err("Frozen outputs exceed configured bounds".into());
            }
            rows.extend(chunk);
            if page["complete"] == true {
                break;
            }
            continuation = page["continuation"].clone();
            if continuation.is_null() {
                return Err("Incomplete output lacks continuation".into());
            }
        }
        let data = encoded(&rows)?;
        outputs.push(FrozenOutput {
            name: name.clone(),
            fields: relation.fields.clone(),
            rows: rows.len(),
            blob: write_blob(&dir, &data)?,
        });
    }
    if instance.backend.revision() != revision {
        return Err("Revision changed during capture".into());
    }
    let manifest = FrozenManifest {
        schema_version: 1,
        key: key.clone(),
        policy: boundary.policy.clone(),
        binding: boundary.binding.clone(),
        revision,
        checkpoint_receipt: json!(receipt),
        checkpoint,
        outputs,
    };
    validate_manifest(&manifest)?;
    let bytes = encoded(&manifest)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("Frozen manifest exceeds bound".into());
    }
    immutable(&dir.join("manifest.json"), &bytes)?;
    Ok(manifest)
}
fn record<'a>(world: &'a World, key: &BoundaryKey) -> Result<&'a admission::Record> {
    let record = world
        .admission
        .records
        .get(&admission::record_key(key.generation, &key.admission_key))
        .ok_or("Unknown boundary admission")?;
    if record.request_sha256 != key.request_sha256 || record.boundary.is_none() {
        return Err("Boundary admission identity mismatch".into());
    }
    Ok(record)
}
pub(super) fn recover(root: &Path, id: &str, world: &mut World) -> Result<()> {
    let Some(pending) = world.admission.external_pending.clone() else {
        return Ok(());
    };
    let record = world
        .admission
        .records
        .get_mut(&pending)
        .ok_or("Missing durable boundary admission")?;
    let boundary = record
        .boundary
        .as_mut()
        .ok_or("Pending admission lacks external boundary")?;
    let key = BoundaryKey {
        world_id: id.into(),
        generation: record.generation,
        admission_key: record.admission_key.clone(),
        request_sha256: record.request_sha256.clone(),
    };
    match read_manifest(root, &key) {
        Ok(Some(manifest)) => {
            if manifest.binding != boundary.binding || manifest.policy != boundary.policy {
                return Err("Durable boundary binding conflict".into());
            }
            record.applied_revision = Some(manifest.revision);
            record.state = "frozen".into();
            record.publication = "pending".into();
            record.error = None;
            boundary.manifest = Some(manifest);
        }
        Ok(None) => {
            record.state = "uncertain".into();
            record.publication = "uncertain".into();
            record.error = Some(
                "Owner exited without complete frozen evidence; input replay is forbidden".into(),
            );
        }
        Err(error) => {
            record.state = "uncertain".into();
            record.publication = "uncertain".into();
            record.error = Some(error);
            boundary.manifest = None;
        }
    }
    world.persistence_dirty = true;
    Ok(())
}
impl WorldManager {
    pub fn admit_boundary_async(&mut self, mut request: BoundaryAdmission) -> Result<Value> {
        admission::normalize_inputs(&mut request.admission);
        admission::validate_request(&request.admission)?;
        validate_binding(&request.binding)?;
        if request.admission.effect.is_some()
            || request.admission.worker_id.is_some()
            || encoded(&request.binding)?.len() > 64 * 1024
        {
            return Err(
                "External boundary requires pure input admission and bounded context".into(),
            );
        }
        let id = &request.admission.id;
        self.poll_boundary(id)?;
        let world = self.worlds.get(id).ok_or("Unknown world")?;
        let policy = world
            .definition
            .external_publication
            .clone()
            .ok_or("World has no external publication policy")?;
        let digest = workers::digest(&json!({"request":request,"policy":policy}))?;
        let key = BoundaryKey {
            world_id: id.clone(),
            generation: request.admission.expected_generation,
            admission_key: request.admission.admission_key.clone(),
            request_sha256: digest.clone(),
        };
        let slot = admission::record_key(key.generation, &key.admission_key);
        if let Some(prior) = world.admission.records.get(&slot) {
            if prior.request_sha256 != digest || prior.boundary.is_none() {
                return Err("Admission key already identifies different contents".into());
            }
            return Ok(prior.result(id, true, false));
        }
        self.ensure_starting_allowed()?;
        self.status_with(id, true)?;
        let world = self.worlds.get(id).unwrap();
        super::fork::guard_admission(world)?;
        if world.admission.external_pending.is_some() || world.boundary_job.is_some() {
            return Err("External publication remains unresolved".into());
        }
        if request.binding.parent_receipt_sha256 != world.admission.external_head {
            return Err("External publication parent mismatch".into());
        }
        let changes = admission::prepare_inputs(world, &request.admission)?;
        let boundary = BoundaryRecord {
            policy,
            binding: request.binding,
            checkpoint_id: crate::bounded::owner_identity(),
            manifest: None,
            external_receipt: None,
        };
        let record = admission::Record {
            generation: key.generation,
            admission_key: key.admission_key.clone(),
            request_sha256: digest,
            effect: None,
            state: "pending".into(),
            applied_revision: None,
            receipt: None,
            error: None,
            publication: "pending".into(),
            at_unix_ms: super::timestamp(),
            boundary: Some(boundary),
        };
        let world = self.worlds.get_mut(id).unwrap();
        world.admission.records.insert(slot.clone(), record);
        world.admission.external_pending = Some(slot);
        world.persistence_dirty = true;
        // If persistence is uncertain, keep the in-memory barrier and never apply.
        persistence::persist(&self.build_root, id, world)?;
        self.launch_boundary(key, Some(changes))?;
        self.admission_status(AdmissionQuery {
            id: id.clone(),
            generation: request.admission.expected_generation,
            admission_key: request.admission.admission_key,
        })
    }
    fn launch_boundary(&mut self, key: BoundaryKey, changes: Option<Value>) -> Result<()> {
        let world = self.worlds.get_mut(&key.world_id).ok_or("Unknown world")?;
        let boundary = record(world, &key)?.boundary.clone().unwrap();
        let mut instance = world
            .instance
            .take()
            .ok_or("No live instance for capture")?;
        let root = self.build_root.clone();
        let work_key = key.clone();
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut revision = if changes.is_none() {
                Some(instance.backend.revision())
            } else {
                None
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> Result<FrozenManifest> {
                    if let Some(changes) = changes {
                        instance
                            .backend
                            .apply_without_deltas(&changes)
                            .map_err(|_| {
                                "Native input acknowledgement lost; replay forbidden".to_string()
                            })?;
                        revision = Some(instance.backend.revision());
                    }
                    freeze(&root, &work_key, &boundary, &mut instance)
                },
            ));
            let result = match outcome {
                Ok(result) => result,
                Err(_) => Err("Boundary worker panicked; reconcile retained evidence".into()),
            };
            let (manifest, error) = match result {
                Ok(m) => (Some(m), None),
                Err(e) => (None, Some(e)),
            };
            let _ = sender.send(Completion {
                instance,
                revision,
                manifest,
                error,
            });
        });
        world.boundary_job = Some(Job {
            key,
            receiver,
            thread: Some(thread),
        });
        Ok(())
    }
    pub(super) fn poll_boundary(&mut self, id: &str) -> Result<()> {
        let world = self.worlds.get_mut(id).ok_or("Unknown world")?;
        let completion = world
            .boundary_job
            .as_ref()
            .map(|job| job.receiver.try_recv());
        let Some(completion) = completion else {
            return Ok(());
        };
        if matches!(completion, Err(mpsc::TryRecvError::Empty)) {
            return Ok(());
        }
        let job = world.boundary_job.take().unwrap();
        let key = job.key.clone();
        drop(job); // Completed/disconnected worker only; no manager lock waits on native I/O.
        let slot = admission::record_key(key.generation, &key.admission_key);
        if world.generation != key.generation
            || world.admission.external_pending.as_deref() != Some(&slot)
        {
            return Err("Late boundary completion does not match retained generation".into());
        }
        let record = world
            .admission
            .records
            .get_mut(&slot)
            .ok_or("Missing boundary record")?;
        match completion {
            Ok(done) => {
                record.applied_revision = done.revision;
                record.error = done.error;
                record.state = if done.manifest.is_some() {
                    "frozen"
                } else if done.revision.is_some() {
                    "applied_but_unpublished"
                } else {
                    "uncertain"
                }
                .into();
                record.publication = if done.manifest.is_some() {
                    "pending"
                } else {
                    "uncertain"
                }
                .into();
                record.boundary.as_mut().unwrap().manifest = done.manifest;
                if !world.stop_requested {
                    world.instance = Some(done.instance)
                } else {
                    drop(done.instance);
                    world.state = "stopped".into()
                }
            }
            Err(_) => {
                record.state = "uncertain".into();
                record.publication = "uncertain".into();
                record.error = Some("Boundary worker exited without acknowledgement".into());
                world.state = if world.stop_requested {
                    "stopped"
                } else {
                    "failed"
                }
                .into();
            }
        }
        world.persistence_dirty = true;
        if let Err(error) = persistence::persist(&self.build_root, id, world) {
            world.persistence.checkpoint_error = Some(error.clone());
            world.admission.records.get_mut(&slot).unwrap().error =
                Some(format!("Completion state persistence failed: {error}"));
            world.persistence_dirty = true;
        }
        Ok(())
    }
    pub fn retry_boundary_freeze_async(&mut self, key: BoundaryKey) -> Result<Value> {
        self.ensure_starting_allowed()?;
        self.poll_boundary(&key.world_id)?;
        let world = self.worlds.get(&key.world_id).ok_or("Unknown world")?;
        let prior = record(world, &key)?;
        if prior.boundary.as_ref().unwrap().manifest.is_some() {
            return Ok(prior.result(&key.world_id, true, false));
        }
        let revision = prior
            .applied_revision
            .ok_or("Uncertain apply cannot retry capture")?;
        if world.stop_requested
            || world.generation != key.generation
            || world.boundary_job.is_some()
            || world.admission.external_pending.as_deref()
                != Some(&admission::record_key(key.generation, &key.admission_key))
            || world
                .instance
                .as_ref()
                .is_none_or(|i| i.backend.revision() != revision || i.backend.health() != "ready")
        {
            return Err("Capture retry requires the exact live acknowledged revision".into());
        }
        let world = self.worlds.get_mut(&key.world_id).unwrap();
        let prior = world
            .admission
            .records
            .get_mut(&admission::record_key(key.generation, &key.admission_key))
            .unwrap();
        prior.state = "pending".into();
        prior.error = None;
        world.persistence_dirty = true;
        persistence::persist(&self.build_root, &key.world_id, world)?;
        self.launch_boundary(key.clone(), None)?;
        self.admission_status(AdmissionQuery {
            id: key.world_id,
            generation: key.generation,
            admission_key: key.admission_key,
        })
    }
    pub fn read_boundary_blob(&mut self, request: FrozenBlobRead) -> Result<FrozenBlobPage> {
        self.poll_boundary(&request.key.world_id)?;
        let world = self
            .worlds
            .get(&request.key.world_id)
            .ok_or("Unknown world")?;
        let manifest = record(world, &request.key)?
            .boundary
            .as_ref()
            .unwrap()
            .manifest
            .as_ref()
            .ok_or("Boundary is not frozen")?;
        if request.max_bytes == 0 || request.max_bytes > crate::MAX_QUERY_BYTES {
            return Err("Frozen page byte limit invalid".into());
        }
        let blob = std::iter::once(&manifest.checkpoint)
            .chain(manifest.outputs.iter().map(|o| &o.blob))
            .find(|b| b.sha256 == request.blob_sha256)
            .ok_or("Blob not in exact frozen manifest")?;
        let bytes = load_blob(&directory(&self.build_root, &request.key)?, blob)?;
        let offset = usize::try_from(request.offset).map_err(|_| "Invalid frozen offset")?;
        if offset > bytes.len() {
            return Err("Frozen offset exceeds blob".into());
        }
        let end = offset.saturating_add(request.max_bytes).min(bytes.len());
        Ok(FrozenBlobPage {
            bytes: bytes[offset..end].to_vec(),
            next_offset: (end < bytes.len()).then_some(end as u64),
        })
    }
    pub fn confirm_boundary_published(
        &mut self,
        key: BoundaryKey,
        receipt: ExternalReceipt,
    ) -> Result<Value> {
        self.poll_boundary(&key.world_id)?;
        let world = self.worlds.get_mut(&key.world_id).ok_or("Unknown world")?;
        let prior = record(world, &key)?;
        let boundary = prior.boundary.as_ref().unwrap();
        let manifest = boundary.manifest.as_ref().ok_or("Boundary is not frozen")?;
        validate_receipt(manifest, &receipt)?;
        if let Some(existing) = &boundary.external_receipt {
            if existing != &receipt {
                return Err("Conflicting external publication acknowledgement".into());
            }
            if prior.state == "published" {
                return Ok(prior.result(&key.world_id, true, false));
            }
            // The exact candidate survived an uncertain durable write. Continue
            // below to atomically persist that same acknowledgement and head.
        }
        let slot = admission::record_key(key.generation, &key.admission_key);
        if world.admission.external_pending.as_deref() != Some(&slot)
            || world.admission.external_head != manifest.binding.parent_receipt_sha256
        {
            return Err("External acknowledgement does not match pending parent".into());
        }
        // Verify retained immutable bytes, independent of native health.
        if read_manifest(&self.build_root, &key)?.as_ref() != Some(manifest) {
            return Err("Frozen manifest changed".into());
        }
        let old_head = world.admission.external_head.clone();
        world.admission.external_head = Some(receipt.receipt_sha256.clone());
        world.admission.external_pending = None;
        let record = world.admission.records.get_mut(&slot).unwrap();
        record.boundary.as_mut().unwrap().external_receipt = Some(receipt);
        record.state = "published".into();
        record.publication = "published".into();
        record.error = None;
        world.persistence_dirty = true;
        if let Err(error) = persistence::persist(&self.build_root, &key.world_id, world) {
            // Disk may contain either state. Keep memory blocked, with the exact
            // candidate retained; the next identical ack retries the transaction.
            world.admission.external_pending = Some(slot.clone());
            world.admission.external_head = old_head;
            let record = world.admission.records.get_mut(&slot).unwrap();
            // Retain the candidate so a conflicting ack cannot steal an
            // uncertain publication in this process or after a status write.
            record.state = "frozen".into();
            record.publication = "pending".into();
            world.persistence_dirty = true;
            return Err(error);
        }
        Ok(world.admission.records[&slot].result(&key.world_id, false, false))
    }
    pub fn restore_boundary_async(&mut self, request: BoundCheckpointRestore) -> Result<Value> {
        if self
            .worlds
            .get(&request.target_world_id)
            .is_some_and(|w| w.fork.as_ref().is_some_and(|f| !f.ready))
        {
            return Err("Unconfirmed fork requires exact reserved restore".into());
        }
        self.restore_bound(request)
    }
    pub(super) fn restore_bound(&mut self, request: BoundCheckpointRestore) -> Result<Value> {
        self.ensure_starting_allowed()?;
        self.status_with(&request.target_world_id, true)?;
        let world = self
            .worlds
            .get(&request.target_world_id)
            .ok_or("Unknown world")?;
        super::creation::guard(world)?;
        let receipt = validate_manifest(&request.manifest)?;
        validate_receipt(&request.manifest, &request.published)?;
        validate_checkpoint_import(
            &self.registry()?,
            &request.manifest,
            &request.checkpoint_bytes,
        )?;
        if world.generation != request.expected_generation
            || world.instance.is_some()
            || world.pending.is_some()
            || world.boundary_job.is_some()
            || world.admission.external_pending.is_some()
            || !matches!(
                world.state.as_str(),
                "created" | "stopped" | "failed" | "interrupted"
            )
            || receipt.program.processor != world.definition.processor
            || world.definition.external_publication.as_ref() != Some(&request.manifest.policy)
            || world
                .admission
                .external_head
                .as_ref()
                .is_some_and(|h| h != &request.published.receipt_sha256)
        {
            return Err("Bound restore target/generation/policy/head mismatch".into());
        }
        // Source/schema/pin checking remains in restore_pinned before compilation.
        let world = self.worlds.get_mut(&request.target_world_id).unwrap();
        world.admission.external_head = Some(request.published.receipt_sha256.clone());
        world.persistence_dirty = true;
        persistence::persist(&self.build_root, &request.target_world_id, world)?;
        self.start_with(
            &request.target_world_id,
            true,
            Some(persistence::Restore {
                receipt,
                source: persistence::RestoreSource::Json(request.checkpoint_bytes),
            }),
        )
    }
}
pub(super) fn validate_receipt(manifest: &FrozenManifest, receipt: &ExternalReceipt) -> Result<()> {
    validate_json(&receipt.receipt, MAX_MANIFEST_BYTES as usize)?;
    if encoded(&receipt.receipt)?.len() > MAX_MANIFEST_BYTES as usize
        || workers::digest(manifest)? != receipt.frozen_manifest_sha256
        || workers::digest(
            &json!({"frozen_manifest_sha256":receipt.frozen_manifest_sha256,"receipt":receipt.receipt}),
        )? != receipt.receipt_sha256
    {
        return Err("External receipt/manifest digest mismatch".into());
    }
    Ok(())
}

pub(super) fn validate_checkpoint_import(
    registry: &crate::registry::ProcessorRegistry,
    manifest: &FrozenManifest,
    bytes: &[u8],
) -> Result<()> {
    let receipt = validate_manifest(manifest)?;
    if bytes.len() > MAX_FROZEN_BYTES
        || hash(bytes) != manifest.checkpoint.sha256
        || bytes.len() as u64 != manifest.checkpoint.bytes
    {
        return Err("Imported checkpoint blob mismatch".into());
    }
    persistence::validate_snapshot(&receipt, bytes)?;
    crate::instance::validate_pinned_checkpoint(registry, bytes, &receipt.program)
}
