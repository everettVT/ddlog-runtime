//! Historical hosted fork identity and readiness belong to this manager, not to
//! an embedding application's input ledger. Reservations precede world.json.
use super::{
    boundary, persistence, workers, BoundCheckpointRestore, ExternalReceipt, FrozenManifest, World,
    WorldDefinition, WorldManager,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

type Result<T> = std::result::Result<T, String>;

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
    pub(super) fn sync_child_directory(self, path: &std::path::Path) -> std::io::Result<()> {
        if matches!(self, Self::ChildDirectorySync) {
            return Err(std::io::Error::other(
                "Injected child directory sync failure",
            ));
        }
        std::fs::File::open(path)?.sync_all()
    }
}

#[derive(Clone, Serialize, Deserialize)]
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
pub(super) struct Record {
    pub schema_version: u32,
    pub reservation: ForkReservation,
    pub definition: WorldDefinition,
    pub manifest: FrozenManifest,
    pub published: ExternalReceipt,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct State {
    pub reservation: ForkReservation,
    pub source: persistence::Receipt,
    pub ready: bool,
}
pub(super) fn guard_admission(world: &World) -> Result<()> {
    super::creation::guard(world)?;
    if world.fork.as_ref().is_some_and(|f| !f.ready) {
        return Err("Fork lineage is not durably confirmed".into());
    }
    Ok(())
}
pub(super) fn guard_start(world: &World, restore: Option<&persistence::Restore>) -> Result<()> {
    super::creation::guard(world)?;
    if let Some(fork) = &world.fork {
        if !fork.ready && restore.is_none_or(|r| r.receipt != fork.source) {
            return Err("Unconfirmed fork requires its exact reserved checkpoint".into());
        }
    }
    Ok(())
}
impl Record {
    pub(super) fn validate(&self) -> Result<persistence::Receipt> {
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
    fn read_fork(&self, request_key: &str) -> Result<Record> {
        self.read_creation(request_key)?
            .fork()
            .cloned()
            .ok_or_else(|| "Creation is not a fork".into())
    }
    pub(super) fn recover_forks(&mut self) -> Result<()> {
        self.recover_creations()
    }
    pub(super) fn checked_fork_record(
        &self,
        request: ForkRequest,
        child: String,
    ) -> Result<Record> {
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
        for context in [&request.destination, &request.source_context] {
            boundary::validate_binding(&super::PublicationBinding {
                context: context.clone(),
                parent_receipt_sha256: None,
            })?;
        }
        let digest = request_digest(
            &request.request_key,
            &request.destination,
            &request.source_context,
            &request.definition,
            &request.manifest,
            &request.published,
        )?;
        let record = Record {
            schema_version: 1,
            reservation: ForkReservation {
                request_key: request.request_key,
                request_sha256: digest,
                child_world_id: child,
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
        Ok(record)
    }
    /// Legacy V1 wrapper over the shared catalog and materialization primitive.
    pub fn reserve_fork(&mut self, request: ForkRequest) -> Result<ForkReservation> {
        self.reserve_fork_with_fault(request, ForkFault::None)
    }
    pub fn reserve_fork_with_fault(
        &mut self,
        request: ForkRequest,
        fault: ForkFault,
    ) -> Result<ForkReservation> {
        let candidate = self.checked_fork_record(request, crate::bounded::owner_identity())?;
        let key = &candidate.reservation.request_key;
        let record = if self
            .creation_path(key)?
            .try_exists()
            .map_err(|e| e.to_string())?
        {
            let prior = self.read_creation(key)?;
            let super::creation::CatalogRecord::Legacy(legacy) = &prior else {
                return Err("Request key already binds logical creation".into());
            };
            if legacy.reservation.request_sha256 != candidate.reservation.request_sha256 {
                return Err("Fork request key identifies different contents".into());
            }
            prior
        } else {
            let record = super::creation::CatalogRecord::Legacy(Box::new(candidate));
            self.save_creation(&record)?;
            record
        };
        if matches!(fault, ForkFault::AfterReservation) {
            return Err("Injected failure after durable fork reservation".into());
        }
        self.materialize_creation(&record, fault)?;
        Ok(record.fork().unwrap().reservation.clone())
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
