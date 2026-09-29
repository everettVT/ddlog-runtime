//! Durable world records and managed checkpoint publications. Receipt paths are always
//! derived from private owner state; clients supply an exact receipt, never a path.
use super::{timestamp, World, WorldManager};
use crate::instance::ProgramIdentity;
use crate::registry::ProcessorRegistry;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, String>;
const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Origin {
    pub world_id: String,
    pub generation: u64,
    pub revision: u64,
    pub program_version: u64,
    /// Provenance of the actual origin executable, not a promise that a rebuilt
    /// executable is byte-identical under another native toolchain.
    pub build: Value,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub schema_version: u32,
    pub format: String,
    pub receipt_id: String,
    pub program: ProgramIdentity,
    pub origin: Origin,
    pub checkpoint_sha256: String,
    pub published_at_unix_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<super::storage::Location>,
}
impl Receipt {
    pub fn metadata(&self) -> Value {
        json!({"world_checkpoint":{"schema_version":self.schema_version,
            "receipt_id":self.receipt_id,"program":self.program,"origin":self.origin}})
    }
    pub fn validate(&self) -> Result<()> {
        component(&self.receipt_id)?;
        component(&self.origin.world_id)?;
        if self.schema_version != 1
            || !matches!(self.format.as_str(), "json" | "iceberg")
            || (self.format == "json" && self.storage.is_some())
            || (self.format == "iceberg" && self.storage.is_none())
            || self.origin.generation == 0
            || self.origin.program_version == 0
            || self.origin.revision < self.origin.program_version
            || self.checkpoint_sha256.len() != 64
            || !self
                .checkpoint_sha256
                .bytes()
                .all(|c| c.is_ascii_hexdigit())
            || self.published_at_unix_ms == Some(0)
        {
            return Err("Invalid managed checkpoint receipt".into());
        }
        Ok(())
    }
}

/// Additive defaults keep old world records readable. Publications themselves
/// live separately so a failed world.json write cannot erase a durable receipt.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Persistence {
    pub checkpoint_error: Option<String>,
    pub restore_requested: Option<Receipt>,
    pub restored_from: Option<Receipt>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Publication {
    pub status: String,
    pub receipt: Receipt,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<Value>,
}

/// Validated bytes are owned by the asynchronous start thread. There is no
/// second filesystem read between verification and replay.
pub(super) struct Restore {
    pub receipt: Receipt,
    pub source: RestoreSource,
}
pub(super) enum RestoreSource {
    Json(Vec<u8>),
    #[cfg(feature = "iceberg")]
    Iceberg(super::storage::Read),
}
impl Restore {
    pub fn install(
        self,
        instance: &mut crate::ProgramInstance,
    ) -> Result<super::admission::Effects> {
        #[cfg(not(feature = "iceberg"))]
        let RestoreSource::Json(bytes) = self.source;
        #[cfg(feature = "iceberg")]
        let bytes = match self.source {
            RestoreSource::Json(bytes) => bytes,
            #[cfg(feature = "iceberg")]
            RestoreSource::Iceberg(read) => read.load(instance.backend.control.clone())?,
        };
        let effects = validate_snapshot(&self.receipt, &bytes)?;
        instance.restore_pinned(&bytes, &self.receipt.program)?;
        Ok(effects)
    }
}
pub(super) fn validate_snapshot(
    receipt: &Receipt,
    bytes: &[u8],
) -> Result<super::admission::Effects> {
    let (digest, state) = crate::checkpoint::inspect(bytes)?;
    if digest != receipt.checkpoint_sha256
        || state.metadata["world_checkpoint"] != receipt.metadata()["world_checkpoint"]
        || state
            .metadata
            .as_object()
            .is_none_or(|m| m.keys().any(|k| k != "world_checkpoint" && k != "effects"))
        || state.revision != receipt.origin.revision
        || state.program_version != receipt.origin.program_version
    {
        return Err("Checkpoint digest, origin or metadata does not match receipt".into());
    }
    super::admission::validate_effects(state.metadata.get("effects"))
}

pub(super) fn component(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id.bytes().all(|b| b.is_ascii_digit() || b == b'-')
        || !id.bytes().any(|b| b.is_ascii_digit())
    {
        return Err("Invalid relative world/receipt ID".into());
    }
    Ok(())
}
pub(super) fn private_dir(path: &Path) -> Result<()> {
    // Reuse the existing same-user, non-symlink, owner-private directory contract.
    ProcessorRegistry::open(path.to_path_buf()).map(|_| ())
}
pub(super) fn existing_dir(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("Managed persistence directory is missing or is a symlink".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err("Managed persistence directory must be owner-private".into());
        }
    }
    Ok(())
}
pub(super) fn checkpoint_root(root: &Path, id: &str) -> Result<PathBuf> {
    component(id)?;
    let world = root.join(id);
    // World directories in older stores inherit the private build root's
    // protection, so check their type but do not impose a new permission format.
    if !fs::symlink_metadata(&world)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err("Invalid origin world directory".into());
    }
    Ok(world.join("checkpoints"))
}
pub(super) fn receipt_dir(root: &Path, receipt: &Receipt) -> Result<PathBuf> {
    receipt.validate()?;
    let checkpoints = checkpoint_root(root, &receipt.origin.world_id)?;
    existing_dir(&checkpoints)?;
    let dir = checkpoints.join(&receipt.receipt_id);
    existing_dir(&dir)?;
    Ok(dir)
}
pub(super) fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() > max {
        return Err("Invalid or oversized managed persistence file".into());
    }
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > max {
        return Err("Managed persistence file exceeds size limit".into());
    }
    Ok(bytes)
}
pub(super) fn read_publication(dir: &Path) -> Result<Publication> {
    let publication: Publication = serde_json::from_slice(&read_bounded(
        &dir.join("publication.json"),
        MAX_RECEIPT_BYTES,
    )?)
    .map_err(|e| e.to_string())?;
    publication.receipt.validate()?;
    if !matches!(
        publication.status.as_str(),
        "staging" | "staged" | "publishing" | "published" | "failed" | "uncertain"
    ) {
        return Err("Invalid checkpoint publication state".into());
    }
    if publication.status == "published" && publication.receipt.published_at_unix_ms.is_none() {
        return Err("Published receipt has no publication time".into());
    }
    Ok(publication)
}

impl WorldManager {
    /// Synchronous bounded snapshot/publication (64 MiB JSON format limit).
    /// A receipt is returned only after both checkpoint and publication fsync.
    pub fn checkpoint(&mut self, id: &str) -> Result<Value> {
        self.status_with(id, true)?;
        let result = self.publish_checkpoint(id);
        let world = self.worlds.get_mut(id).ok_or("Unknown world")?;
        world.persistence.checkpoint_error = result.as_ref().err().cloned();
        world.persistence_dirty = true;
        // Receipt publication is independently durable. A world-record error
        // cannot turn it into a returned success or discard its history.
        persist(&self.build_root, id, world)?;
        result
    }
    fn publish_checkpoint(&self, id: &str) -> Result<Value> {
        let world = self.worlds.get(id).ok_or("Unknown world")?;
        let (receipt, bytes) = freeze(world, id, "json")?;
        let checkpoints = checkpoint_root(&self.build_root, id)?;
        private_dir(&checkpoints)?;
        let dir = checkpoints.join(&receipt.receipt_id);
        fs::create_dir(&dir).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
        File::open(&checkpoints)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        File::open(self.build_root.join(id))
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        let mut publication = Publication {
            status: "staging".into(),
            receipt,
            error: None,
            staged: None,
        };
        let manifest = dir.join("publication.json");
        let write_publication = |p: &Publication| write_record(&manifest, p);
        write_publication(&publication)?;
        let result = (|| {
            atomic_bytes(&dir.join("checkpoint.json"), &bytes)?;
            publication.status = "staged".into();
            write_publication(&publication)?;
            publication.status = "published".into();
            publication.receipt.published_at_unix_ms = Some(timestamp());
            write_publication(&publication)
        })();
        if let Err(error) = result {
            publication.status = if dir.join("checkpoint.json").exists() {
                "uncertain"
            } else {
                "failed"
            }
            .into();
            publication.receipt.published_at_unix_ms = None;
            publication.error = Some(error.clone());
            let _ = write_publication(&publication);
            return Err(format!(
                "Checkpoint publication failed; inspect persistence before retrying: {error}"
            ));
        }
        Ok(json!(publication.receipt))
    }

    /// Restore is explicit and asynchronous. The immutable receipt must be
    /// published in this owner's private store; another world may use it only
    /// with the identical processor pin. Ordinary Start never consults it.
    pub fn restore_async(&mut self, id: &str, receipt: &Value) -> Result<Value> {
        self.ensure_starting_allowed()?;
        self.status_with(id, true)?;
        let world = self.worlds.get(id).ok_or("Unknown world")?;
        if !matches!(
            world.state.as_str(),
            "created" | "stopped" | "failed" | "interrupted"
        ) || world.instance.is_some()
            || world.pending.is_some()
        {
            return Err("Restore requires a created, stopped, failed or interrupted world with no live/pending instance".into());
        }
        let receipt: Receipt =
            serde_json::from_value(receipt.clone()).map_err(|e| e.to_string())?;
        if receipt.program.processor != world.definition.processor {
            return Err("Restore receipt processor pin does not match target world".into());
        }
        let dir = receipt_dir(&self.build_root, &receipt)?;
        let publication = read_publication(&dir)?;
        if publication.status != "published" || publication.receipt != receipt {
            return Err("Restore requires the exact immutable published receipt".into());
        }
        let source = if receipt.format == "json" {
            let bytes = read_bounded(&dir.join("checkpoint.json"), crate::checkpoint::MAX_BYTES)?;
            validate_snapshot(&receipt, &bytes)?;
            RestoreSource::Json(bytes)
        } else {
            self.iceberg_restore_source(&publication)?
        };
        self.start_with(id, true, Some(Restore { receipt, source }))
    }
}

/// Shared identity and input freeze. Storage adapters never reconstruct inputs.
pub(super) fn freeze(world: &World, id: &str, format: &str) -> Result<(Receipt, Vec<u8>)> {
    if world.state != "running" || world.pending.is_some() {
        return Err("Checkpoint requires a running world without pending lifecycle work".into());
    }
    let instance = world.instance.as_ref().ok_or("World is not running")?;
    let program = instance.checkpoint_identity()?;
    let mut receipt = Receipt {
        schema_version: 1,
        format: format.into(),
        storage: None,
        receipt_id: crate::bounded::owner_identity(),
        origin: Origin {
            world_id: id.into(),
            generation: world.generation,
            revision: instance.backend.revision(),
            program_version: instance.backend.version,
            build: instance.backend.build_identity(),
        },
        program,
        checkpoint_sha256: String::new(),
        published_at_unix_ms: None,
    };
    // Backend owns format validation and the acknowledged input snapshot,
    // including empty inputs and format limits. No output/provider replay.
    let mut metadata = receipt.metadata();
    if !world.admission.effects.is_empty() {
        metadata["effects"] =
            serde_json::to_value(&world.admission.effects).map_err(|e| e.to_string())?;
    }
    let bytes = instance.backend.checkpoint_bytes(metadata)?;
    receipt.checkpoint_sha256 = crate::checkpoint::inspect(&bytes)?.0;
    Ok((receipt, bytes))
}
pub(super) fn write_record(path: &Path, p: &Publication) -> Result<()> {
    let value = serde_json::to_value(p).map_err(|e| e.to_string())?;
    if serde_json::to_vec(&value).map_err(|e| e.to_string())?.len() as u64 > MAX_RECEIPT_BYTES {
        return Err("Managed receipt exceeds 1 MiB limit".into());
    }
    atomic_json(path, &value)
}

/// Availability is a cheap filesystem observation, never a claim of verified
/// contents or valid registry dependencies. Restore verifies both before launch.
pub(super) fn status(root: &Path, id: &str, world: &World, revision: &Value) -> Value {
    let mut checkpoints = Vec::new();
    let entries = (|| -> Result<_> {
        let path = checkpoint_root(root, id)?;
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
            Ok(_) => (),
        }
        existing_dir(&path)?;
        Ok(Some(fs::read_dir(path).map_err(|e| e.to_string())?))
    })();
    let mut error = world.persistence.checkpoint_error.clone();
    match entries {
        Err(e) => error = Some(e),
        Ok(None) => (),
        Ok(Some(entries)) => {
            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        error = Some(e.to_string());
                        continue;
                    }
                };
                let result = (|| -> Result<Publication> {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    component(&name)?;
                    existing_dir(&entry.path())?;
                    let publication = read_publication(&entry.path())?;
                    if publication.receipt.receipt_id != name
                        || publication.receipt.origin.world_id != id
                    {
                        return Err("Receipt location/origin mismatch".into());
                    }
                    Ok(publication)
                })();
                match result {
                Err(e) => checkpoints.push(json!({"receipt_id":entry.file_name().to_string_lossy(),"status":"unavailable","availability":"invalid","error":e})),
                Ok(p) => {
                    let present = fs::symlink_metadata(entry.path().join("checkpoint.json"))
                        .is_ok_and(|m| m.is_file() && m.len() <= crate::checkpoint::MAX_BYTES);
                    checkpoints.push(json!({"receipt":p.receipt,"status":p.status,"error":p.error,
                        "availability":if p.receipt.format == "iceberg" {"catalog_unverified"} else if present {"present_unverified"} else {"missing_or_invalid"}}));
                }
            }
            }
        }
    }
    checkpoints.sort_by_key(|p| p["receipt"]["published_at_unix_ms"].as_u64().unwrap_or(0));
    let persisted_revision = checkpoints
        .iter()
        .filter(|p| {
            p["status"] == "published"
                && p["availability"] == "present_unverified"
                && p["receipt"]["origin"]["generation"] == world.generation
        })
        .filter_map(|p| p["receipt"]["origin"]["revision"].as_u64())
        .chain(
            world
                .persistence
                .restored_from
                .iter()
                .filter(|r| r.format == "json")
                .map(|r| r.origin.revision),
        )
        .max();
    json!({"schema_version":1,"status":if error.is_some() {"error"} else {"configured"}, "format":"json",
        "coverage":"whole_pure_program_inputs", "committed_revision":revision,
        "persisted_revision":persisted_revision,"checkpoints":checkpoints,"error":error,
        "restore_requested":world.persistence.restore_requested,"restored_from":world.persistence.restored_from,
        "verification":"Receipt/object presence only; restore verifies integrity and pinned dependencies before compilation"})
}

pub(super) fn persist_if_changed(
    root: &std::path::Path,
    id: &str,
    world: &mut World,
) -> Result<()> {
    if world.persistence_dirty
        || world.history.last().is_none_or(|last| {
            last["state"] != world.state
                || last["generation"] != world.generation
                || last["error"] != json!(world.error)
        })
    {
        persist(root, id, world)?;
    }
    Ok(())
}
pub(super) fn persist(root: &std::path::Path, id: &str, world: &mut World) -> Result<()> {
    let mut history = world.history.clone();
    if history.last().is_none_or(|last| {
        last["state"] != world.state
            || last["generation"] != world.generation
            || last["error"] != json!(world.error)
    }) {
        history.push(json!({"at_unix_ms":timestamp(),"state":world.state,"generation":world.generation,"error":world.error,"restore_requested":world.persistence.restore_requested,"restored_from":world.persistence.restored_from}));
    }
    let path = root.join(id).join("world.json");
    let mut record = serde_json::to_value(&*world).map_err(|e| e.to_string())?;
    record["history"] = json!(history);
    atomic_json(&path, &record)?;
    world.history = history;
    world.persistence_dirty = false;
    Ok(())
}

pub(super) fn atomic_json(path: &Path, value: &Value) -> Result<()> {
    atomic_bytes(path, &serde_json::to_vec(value).map_err(|e| e.to_string())?)
}
fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("Missing persistence parent")?;
    let temp = path.with_extension("json.tmp");
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temp).map_err(|e| e.to_string())?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(temp);
        return Err(format!(
            "Durable publication failed (target may exist after rename): {error}"
        ));
    }
    Ok(())
}
