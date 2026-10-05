//! Historical hosted fork identity and readiness belong to this manager, not to
//! an embedding application's input ledger. Reservations precede world.json.
use super::{
    boundary, persistence, workers, BoundCheckpointRestore, ExternalReceipt, FrozenManifest, World,
    WorldDefinition, WorldManager,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

type Result<T> = std::result::Result<T, String>;
const MAX_RECORD: u64 = 2 * 1024 * 1024;
const MAX_RESERVATIONS: usize = 1024;

/// Deterministic durability fault seam. Normal callers use `reserve_fork`.
#[derive(Clone, Copy, Default)]
pub enum ForkFault {
    #[default]
    None,
    AfterReservation,
    AfterChildRecord,
    ChildDirectorySync,
}
impl ForkFault {
    fn sync_child_directory(self, path: &std::path::Path) -> std::io::Result<()> {
        if matches!(self, Self::ChildDirectorySync) {
            return Err(std::io::Error::other(
                "Injected child directory sync failure",
            ));
        }
        std::fs::File::open(path)?.sync_all()
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkRequest {
    pub request_key: String,
    /// Immutable, bounded destination identity chosen by trusted composition.
    pub destination: Value,
    pub source_context: Value,
    pub definition: WorldDefinition,
    pub manifest: FrozenManifest,
    pub published: ExternalReceipt,
    pub checkpoint_bytes: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkReservation {
    pub request_key: String,
    pub request_sha256: String,
    pub child_world_id: String,
    pub destination: Value,
    pub source_context: Value,
    pub source_manifest_sha256: String,
    pub source_receipt_sha256: String,
}
impl ForkReservation {
    /// The storage origin must bind this complete reservation, including both
    /// source proof and destination identity. Confirmation is an explicit ack.
    pub fn lineage_sha256(&self) -> Result<String> {
        workers::digest(&json!({"schema_version":1,"fork":self}))
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema_version: u32,
    reservation: ForkReservation,
    definition: WorldDefinition,
    manifest: FrozenManifest,
    published: ExternalReceipt,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct State {
    pub reservation: ForkReservation,
    pub source: persistence::Receipt,
    pub ready: bool,
}
pub(super) fn guard_admission(world: &World) -> Result<()> {
    if world.fork.as_ref().is_some_and(|f| !f.ready) {
        return Err("Fork lineage is not durably confirmed".into());
    }
    Ok(())
}
pub(super) fn guard_start(world: &World, restore: Option<&persistence::Restore>) -> Result<()> {
    if let Some(fork) = &world.fork {
        if !fork.ready && restore.is_none_or(|r| r.receipt != fork.source) {
            return Err("Unconfirmed fork requires its exact reserved checkpoint".into());
        }
    }
    Ok(())
}
impl Record {
    fn validate(&self) -> Result<persistence::Receipt> {
        workers::token(&self.reservation.request_key)?;
        persistence::component(&self.reservation.child_world_id)?;
        let source = boundary::validate_manifest(&self.manifest)?;
        boundary::validate_receipt(&self.manifest, &self.published)?;
        if self.schema_version != 1
            || self.reservation.source_manifest_sha256 != self.published.frozen_manifest_sha256
            || self.reservation.source_receipt_sha256 != self.published.receipt_sha256
            || self.reservation.request_sha256
                != request_digest(
                    &self.reservation.request_key,
                    &self.reservation.destination,
                    &self.reservation.source_context,
                    &self.definition,
                    &self.manifest,
                    &self.published,
                )?
            || self.definition.processor != source.program.processor
            || self.definition.external_publication.as_ref() != Some(&self.manifest.policy)
            || self.reservation.child_world_id == source.origin.world_id
        {
            return Err("Invalid fork reservation identity".into());
        }
        Ok(source)
    }
}
fn request_digest(
    key: &str,
    destination: &Value,
    source_context: &Value,
    definition: &WorldDefinition,
    manifest: &FrozenManifest,
    published: &ExternalReceipt,
) -> Result<String> {
    workers::digest(
        &json!({"request_key":key,"destination":destination,"source_context":source_context,
        "definition":definition,"manifest":manifest,"published":published}),
    )
}
impl WorldManager {
    fn fork_path(&self, request_key: &str) -> Result<PathBuf> {
        workers::token(request_key)?;
        Ok(self
            .build_root
            .join("forks")
            .join(format!("{}.json", workers::digest(&json!(request_key))?)))
    }
    fn read_fork(&self, request_key: &str) -> Result<Record> {
        let record: Record = serde_json::from_slice(&persistence::read_bounded(
            &self.fork_path(request_key)?,
            MAX_RECORD,
        )?)
        .map_err(|e| e.to_string())?;
        record.validate()?;
        if record.reservation.request_key != request_key {
            return Err("Fork key mismatch".into());
        }
        Ok(record)
    }
    fn materialization_path(&self, request_key: &str) -> Result<PathBuf> {
        Ok(self.fork_path(request_key)?.with_extension("materialized"))
    }
    fn fence_materialization(
        &self,
        record: &Record,
        world: &World,
        fault: ForkFault,
    ) -> Result<()> {
        // The retained rename may have succeeded while its directory sync
        // failed. Repeat these fences without rewriting the original record,
        // even when recovery already loaded it or the marker already exists.
        let child_dir = self.build_root.join(&record.reservation.child_world_id);
        std::fs::File::open(child_dir.join("world.json"))
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        fault
            .sync_child_directory(&child_dir)
            .map_err(|e| e.to_string())?;
        std::fs::File::open(&self.build_root)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        let marker = self.materialization_path(&record.reservation.request_key)?;
        if marker.exists() {
            let saved: ForkReservation =
                serde_json::from_slice(&persistence::read_bounded(&marker, MAX_RECORD)?)
                    .map_err(|e| e.to_string())?;
            if saved != record.reservation {
                return Err("Fork materialization identity changed".into());
            }
            for path in [marker.as_path(), marker.parent().unwrap()] {
                std::fs::File::open(path)
                    .and_then(|f| f.sync_all())
                    .map_err(|e| e.to_string())?;
            }
        } else {
            if world.generation != 0
                || world.state != "created"
                || !world.admission.records.is_empty()
                || world.admission.external_pending.is_some()
                || world.admission.external_head.is_some()
                || world.fork.as_ref().is_none_or(|f| f.ready)
                || world.persistence.restore_requested.is_some()
                || world.persistence.restored_from.is_some()
            {
                return Err("Progressed fork has no materialization fence".into());
            }
            persistence::atomic_json(
                &marker,
                &serde_json::to_value(&record.reservation).map_err(|e| e.to_string())?,
            )?;
        }
        Ok(())
    }
    fn materialize_fork(&mut self, record: &Record, fault: ForkFault) -> Result<()> {
        // Repeat every durability fence after an ambiguous rename/fsync result.
        let path = self.fork_path(&record.reservation.request_key)?;
        for path in [
            path.as_path(),
            path.parent().unwrap(),
            self.build_root.as_path(),
        ] {
            std::fs::File::open(path)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
        }
        let source = record.validate()?;
        let id = &record.reservation.child_world_id;
        if let Some(world) = self.worlds.get(id) {
            if world
                .fork
                .as_ref()
                .is_none_or(|f| f.reservation != record.reservation || f.source != source)
                || serde_json::to_value(&world.definition).map_err(|e| e.to_string())?
                    != serde_json::to_value(&record.definition).map_err(|e| e.to_string())?
            {
                return Err("Reserved fork child changed".into());
            }
            return self.fence_materialization(record, world, fault);
        }
        let dir = self.build_root.join(id);
        let world_path = dir.join("world.json");
        if !world_path.exists() {
            if self
                .materialization_path(&record.reservation.request_key)?
                .exists()
            {
                return Err("Materialized fork has lost its world control record".into());
            }
            if dir.exists() {
                for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
                    if entry.map_err(|e| e.to_string())?.file_name() != "world.json.tmp" {
                        return Err("Fork progress evidence without world control record".into());
                    }
                }
            }
        } else {
            // A prior write may have reached disk before its acknowledgment.
            // Preserve it; never reconstruct defaults over a retained record.
            let world: World =
                serde_json::from_slice(&persistence::read_bounded(&world_path, MAX_RECORD)?)
                    .map_err(|e| e.to_string())?;
            if world.schema_version != super::SCHEMA_VERSION
                || world
                    .fork
                    .as_ref()
                    .is_none_or(|f| f.reservation != record.reservation || f.source != source)
                || serde_json::to_value(&world.definition).map_err(|e| e.to_string())?
                    != serde_json::to_value(&record.definition).map_err(|e| e.to_string())?
            {
                return Err("Reserved fork child changed".into());
            }
            self.fence_materialization(record, &world, fault)?;
            self.worlds.insert(id.clone(), world);
            return Ok(());
        }
        let pin = self.validate_world_definition(&record.definition)?;
        let mut world = self.new_world(record.definition.clone(), &pin)?;
        world.fork = Some(State {
            reservation: record.reservation.clone(),
            source,
            ready: false,
        });
        persistence::private_dir(&dir)?;
        std::fs::File::open(&self.build_root)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        // Never remove a reserved identity after an ambiguous persistence result.
        persistence::persist_with_directory_sync(&self.build_root, id, &mut world, |dir| {
            fault.sync_child_directory(dir)
        })?;
        if matches!(fault, ForkFault::AfterChildRecord) {
            return Err("Injected failure before materialization fence".into());
        }
        self.fence_materialization(record, &world, fault)?;
        self.worlds.insert(id.clone(), world);
        Ok(())
    }
    pub(super) fn recover_forks(&mut self) -> Result<()> {
        let dir = self.build_root.join("forks");
        if !dir.exists() {
            if self.worlds.values().any(|w| w.fork.is_some()) {
                return Err("Fork worlds have no reservation catalog".into());
            }
            return Ok(());
        }
        persistence::existing_dir(&dir)?;
        let mut destinations = std::collections::BTreeSet::new();
        let mut children = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let record: Record =
                serde_json::from_slice(&persistence::read_bounded(&path, MAX_RECORD)?)
                    .map_err(|e| e.to_string())?;
            record.validate()?;
            if path != self.fork_path(&record.reservation.request_key)?
                || !destinations.insert(workers::digest(&record.reservation.destination)?)
                || !children.insert(record.reservation.child_world_id.clone())
            {
                return Err("Conflicting fork reservation catalog".into());
            }
            if children.len() > MAX_RESERVATIONS {
                return Err("Fork reservation catalog limit".into());
            }
            self.materialize_fork(&record, ForkFault::None)?;
        }
        for world in self.worlds.values() {
            if world
                .fork
                .as_ref()
                .is_some_and(|f| !children.contains(&f.reservation.child_world_id))
            {
                return Err("Fork world has no durable reservation".into());
            }
        }
        Ok(())
    }
    /// Fully validate the immutable source before any child/catalog effects.
    /// Repeating a key only materializes the same reserved child; never starts it.
    pub fn reserve_fork(&mut self, request: ForkRequest) -> Result<ForkReservation> {
        self.reserve_fork_with_fault(request, ForkFault::None)
    }
    pub fn reserve_fork_with_fault(
        &mut self,
        request: ForkRequest,
        fault: ForkFault,
    ) -> Result<ForkReservation> {
        workers::token(&request.request_key)?;
        self.validate_world_definition(&request.definition)?;
        let source = boundary::validate_manifest(&request.manifest)?;
        boundary::validate_receipt(&request.manifest, &request.published)?;
        boundary::validate_checkpoint_import(
            &self.registry()?,
            &request.manifest,
            &request.checkpoint_bytes,
        )?;
        if request.definition.processor != source.program.processor
            || request.definition.external_publication.as_ref() != Some(&request.manifest.policy)
        {
            return Err("Fork definition does not match source".into());
        }
        // Reuse the context depth/byte contract; no arbitrary unbounded metadata.
        boundary::validate_binding(&super::PublicationBinding {
            context: request.destination.clone(),
            parent_receipt_sha256: None,
        })?;
        boundary::validate_binding(&super::PublicationBinding {
            context: request.source_context.clone(),
            parent_receipt_sha256: None,
        })?;
        let digest = request_digest(
            &request.request_key,
            &request.destination,
            &request.source_context,
            &request.definition,
            &request.manifest,
            &request.published,
        )?;
        let path = self.fork_path(&request.request_key)?;
        let record = if path.exists() {
            let prior = self.read_fork(&request.request_key)?;
            if prior.reservation.request_sha256 != digest {
                return Err("Fork request key identifies different contents".into());
            }
            prior
        } else {
            self.ensure_starting_allowed()?;
            self.recover_forks()?;
            if self.worlds.values().filter(|w| w.fork.is_some()).count() >= MAX_RESERVATIONS {
                return Err("Fork reservation catalog limit".into());
            }
            if self.worlds.values().any(|w| {
                w.fork
                    .as_ref()
                    .is_some_and(|f| f.reservation.destination == request.destination)
            }) {
                return Err("Fork destination already reserved".into());
            }
            let record = Record {
                schema_version: 1,
                reservation: ForkReservation {
                    request_key: request.request_key,
                    request_sha256: digest,
                    child_world_id: crate::bounded::owner_identity(),
                    destination: request.destination,
                    source_context: request.source_context,
                    source_manifest_sha256: request.published.frozen_manifest_sha256.clone(),
                    source_receipt_sha256: request.published.receipt_sha256.clone(),
                },
                definition: request.definition,
                manifest: request.manifest,
                published: request.published,
            };
            record.validate()?;
            let value = serde_json::to_value(&record).map_err(|e| e.to_string())?;
            if serde_json::to_vec(&value).map_err(|e| e.to_string())?.len() as u64 > MAX_RECORD {
                return Err("Fork reservation exceeds size limit".into());
            }
            persistence::private_dir(path.parent().unwrap())?;
            // fsync parent creation before relying on a durable reservation.
            std::fs::File::open(&self.build_root)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
            persistence::atomic_json(&path, &value)?;
            record
        };
        if matches!(fault, ForkFault::AfterReservation) {
            return Err("Injected failure after durable fork reservation".into());
        }
        self.materialize_fork(&record, fault)?;
        Ok(record.reservation)
    }
    /// Explicit generation-fenced restore, separate from reservation retries.
    /// A duplicate in-flight/completed restore is a lookup, never input replay.
    pub fn restore_fork_async(
        &mut self,
        reservation: ForkReservation,
        expected_generation: u64,
        checkpoint_bytes: Vec<u8>,
    ) -> Result<Value> {
        let record = self.read_fork(&reservation.request_key)?;
        if record.reservation != reservation {
            return Err("Fork reservation changed".into());
        }
        boundary::validate_checkpoint_import(
            &self.registry()?,
            &record.manifest,
            &checkpoint_bytes,
        )?;
        self.status_with(&reservation.child_world_id, true)?;
        let world = self
            .worlds
            .get(&reservation.child_world_id)
            .ok_or("Unknown fork child")?;
        let source = record.validate()?;
        if world.admission.external_pending.is_some()
            || !world.admission.records.is_empty()
            || world
                .admission
                .external_head
                .as_ref()
                .is_some_and(|h| h != &record.published.receipt_sha256)
        {
            return Err("Fork restore cannot replace child input history".into());
        }
        if world.generation
            == expected_generation
                .checked_add(1)
                .ok_or("Generation overflow")?
            && world.persistence.restore_requested.as_ref() == Some(&source)
        {
            return self.status(&reservation.child_world_id);
        }
        self.restore_bound(BoundCheckpointRestore {
            target_world_id: reservation.child_world_id,
            expected_generation,
            manifest: record.manifest,
            published: record.published,
            checkpoint_bytes,
        })
    }
    /// Explicit acknowledgement of the exact storage-owned lineage identity.
    pub fn confirm_fork_lineage(
        &mut self,
        reservation: ForkReservation,
        lineage_sha256: String,
    ) -> Result<Value> {
        let record = self.read_fork(&reservation.request_key)?;
        if record.reservation != reservation || lineage_sha256 != reservation.lineage_sha256()? {
            return Err("Fork lineage confirmation mismatch".into());
        }
        self.status_with(&reservation.child_world_id, true)?;
        let world = self
            .worlds
            .get_mut(&reservation.child_world_id)
            .ok_or("Unknown fork child")?;
        let fork = world.fork.as_mut().ok_or("Child is not a fork")?;
        if fork.reservation != reservation {
            return Err("Fork reservation changed".into());
        }
        if fork.ready {
            return self.status(&reservation.child_world_id);
        }
        if world.persistence.restored_from.as_ref() != Some(&fork.source) {
            return Err("Exact fork restore has not completed".into());
        }
        if !fork.ready {
            fork.ready = true;
            world.persistence_dirty = true;
            if let Err(e) =
                persistence::persist(&self.build_root, &reservation.child_world_id, world)
            {
                world.fork.as_mut().unwrap().ready = false;
                world.persistence_dirty = true;
                return Err(e);
            }
        }
        self.status(&reservation.child_world_id)
    }
}
