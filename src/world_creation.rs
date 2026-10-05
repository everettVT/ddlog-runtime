//! The existing reservation catalog/materializer, shared by fresh and fork
//! worlds. V1 fork bytes and hashes retain their original interpretation.
use super::{
    boundary, fork, persistence, workers, ForkFault, ForkRequest, ForkReservation,
    PublicationBinding, World, WorldDefinition, WorldManager,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, String>;
const MAX_RECORD: u64 = 2 * 1024 * 1024;
const MAX_RESERVATIONS: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalDestination {
    pub resource: String,
    pub world: String,
    pub run: String,
}
impl LogicalDestination {
    fn validate(&self) -> Result<()> {
        for value in [&self.resource, &self.world, &self.run] {
            workers::token(value)?;
        }
        Ok(())
    }
    fn scope(&self) -> Value {
        json!({"world":self.world,"run":self.run})
    }
    fn conflicts(&self, other: &Self) -> bool {
        self.resource == other.resource || (self.world == other.world && self.run == other.run)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreationRequest {
    pub request_key: String,
    pub destination: LogicalDestination,
    pub definition: WorldDefinition,
    /// Opaque, bounded host declaration payload; identity is separate from it.
    pub binding: Value,
    pub fork: Option<Box<ForkRequest>>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreationReservation {
    pub schema_version: u32,
    pub request_key: String,
    pub request_sha256: String,
    pub world_id: String,
    pub destination: LogicalDestination,
    pub binding_sha256: String,
    pub kind: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedCreation {
    pub reservation: CreationReservation,
    pub definition: WorldDefinition,
    pub binding: Value,
    pub context_id: Option<String>,
    pub context_confirmed: bool,
    pub fork: Option<ForkReservation>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct State {
    pub reservation: CreationReservation,
    pub context_id: Option<String>,
    pub confirmed: bool,
}
#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextCandidate {
    schema_version: u32,
    reservation: CreationReservation,
    context_id: String,
}
fn context_identity(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}
pub(super) fn guard(world: &World) -> Result<()> {
    if world.creation.as_ref().is_some_and(|s| !s.confirmed) {
        return Err("Logical creation context is not durably confirmed".into());
    }
    Ok(())
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Origin {
    Fresh,
    Fork { record: Box<fork::Record> },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LogicalRecord {
    schema_version: u32,
    reservation: CreationReservation,
    definition: WorldDefinition,
    binding: Value,
    origin: Origin,
}
#[derive(Clone)]
pub(super) enum CatalogRecord {
    Legacy(Box<fork::Record>),
    Logical(Box<LogicalRecord>),
}
enum DestinationIdentity {
    Logical(LogicalDestination),
    Legacy {
        digest: String,
        scope: Option<Value>,
    },
}
struct CatalogIdentity {
    key: String,
    id: String,
    destination: DestinationIdentity,
}
impl CatalogIdentity {
    fn conflicts(&self, other: &Self) -> Result<bool> {
        use DestinationIdentity::*;
        match (&self.destination, &other.destination) {
            (Logical(a), Logical(b)) => Ok(a.conflicts(b)),
            (Legacy { digest: a, .. }, Legacy { digest: b, .. }) => Ok(a == b),
            (Logical(logical), Legacy { scope, .. }) | (Legacy { scope, .. }, Logical(logical)) => {
                let scope = scope.as_ref().ok_or("Legacy fork destination needs an explicit logical mapping before mixed creation")?;
                Ok(*scope == logical.scope())
            }
        }
    }
}
fn request_digest(
    key: &str,
    destination: &LogicalDestination,
    definition: &WorldDefinition,
    binding: &Value,
    origin: &Origin,
) -> Result<String> {
    let source = match origin {
        Origin::Fresh => Value::Null,
        Origin::Fork { record } => json!({"source_context":record.reservation.source_context,
            "manifest":record.manifest,"published":record.published}),
    };
    workers::digest(
        &json!({"schema_version":2,"request_key":key,"destination":destination,
        "definition":definition,"binding":binding,"source":source}),
    )
}
impl CatalogRecord {
    fn identity(&self) -> Result<CatalogIdentity> {
        let destination = match self {
            Self::Logical(record) => {
                DestinationIdentity::Logical(record.reservation.destination.clone())
            }
            Self::Legacy(record) => {
                let value = &record.reservation.destination;
                let scope = if value.as_object().is_some_and(|o| o.len() == 2)
                    && value["world"]
                        .as_str()
                        .is_some_and(|s| workers::token(s).is_ok())
                    && value["run"]
                        .as_str()
                        .is_some_and(|s| workers::token(s).is_ok())
                {
                    Some(value.clone())
                } else {
                    None
                };
                DestinationIdentity::Legacy {
                    digest: workers::digest(value)?,
                    scope,
                }
            }
        };
        Ok(CatalogIdentity {
            key: self.key().into(),
            id: self.id().into(),
            destination,
        })
    }
    fn key(&self) -> &str {
        match self {
            Self::Legacy(r) => &r.reservation.request_key,
            Self::Logical(r) => &r.reservation.request_key,
        }
    }
    fn id(&self) -> &str {
        match self {
            Self::Legacy(r) => &r.reservation.child_world_id,
            Self::Logical(r) => &r.reservation.world_id,
        }
    }
    fn definition(&self) -> &WorldDefinition {
        match self {
            Self::Legacy(r) => &r.definition,
            Self::Logical(r) => &r.definition,
        }
    }
    pub(super) fn fork(&self) -> Option<&fork::Record> {
        match self {
            Self::Legacy(r) => Some(r),
            Self::Logical(record) => match &record.origin {
                Origin::Fork { record } => Some(record),
                Origin::Fresh => None,
            },
        }
    }
    fn value(&self) -> Result<Value> {
        match self {
            Self::Legacy(r) => serde_json::to_value(r),
            Self::Logical(r) => serde_json::to_value(r),
        }
        .map_err(|e| e.to_string())
    }
    fn marker(&self) -> Result<Value> {
        match self {
            Self::Legacy(r) => serde_json::to_value(&r.reservation),
            Self::Logical(r) => serde_json::to_value(&r.reservation),
        }
        .map_err(|e| e.to_string())
    }
    fn validate(&self) -> Result<()> {
        workers::token(self.key())?;
        persistence::component(self.id())?;
        if let Some(fork) = self.fork() {
            fork.validate()?;
        }
        if let Self::Logical(record) = self {
            let r = &record.reservation;
            r.destination.validate()?;
            boundary::validate_binding(&PublicationBinding {
                context: record.binding.clone(),
                parent_receipt_sha256: None,
            })?;
            if record.schema_version != 2
                || r.schema_version != 2
                || record.definition.external_publication.is_none()
                || r.binding_sha256 != workers::digest(&record.binding)?
                || r.request_sha256
                    != request_digest(
                        &r.request_key,
                        &r.destination,
                        &record.definition,
                        &record.binding,
                        &record.origin,
                    )?
                || r.kind
                    != if self.fork().is_some() {
                        "fork"
                    } else {
                        "fresh"
                    }
            {
                return Err("Invalid logical creation reservation".into());
            }
            if let Some(fork) = self.fork() {
                if fork.reservation.request_key != r.request_key
                    || fork.reservation.child_world_id != r.world_id
                    || fork.reservation.destination != r.destination.scope()
                    || serde_json::to_value(&fork.definition).map_err(|e| e.to_string())?
                        != serde_json::to_value(&record.definition).map_err(|e| e.to_string())?
                {
                    return Err("Logical fork source/reservation mismatch".into());
                }
            }
        }
        Ok(())
    }
    fn matches_world(&self, world: &World) -> Result<()> {
        if world.schema_version != super::SCHEMA_VERSION
            || serde_json::to_value(&world.definition).map_err(|e| e.to_string())?
                != serde_json::to_value(self.definition()).map_err(|e| e.to_string())?
        {
            return Err("Reserved world definition changed".into());
        }
        match (self.fork(), &world.fork) {
            (Some(r), Some(s)) if r.reservation == s.reservation && r.validate()? == s.source => (),
            (None, None) => (),
            _ => return Err("Reserved fork source changed".into()),
        }
        match (self, &world.creation) {
            (Self::Legacy(_), None) => (),
            (Self::Logical(r), Some(s))
                if s.reservation == r.reservation
                    && (!s.confirmed || s.context_id.is_some())
                    && s.context_id.as_ref().is_none_or(|id| context_identity(id)) => {}
            _ => return Err("Reserved logical world binding changed".into()),
        }
        Ok(())
    }
    fn initialize(&self, world: &mut World) -> Result<()> {
        if let Some(r) = self.fork() {
            world.fork = Some(fork::State {
                reservation: r.reservation.clone(),
                source: r.validate()?,
                ready: false,
            });
        }
        if let Self::Logical(r) = self {
            world.creation = Some(State {
                reservation: r.reservation.clone(),
                context_id: None,
                confirmed: false,
            });
        }
        Ok(())
    }
}

fn read_record(path: &Path) -> Result<CatalogRecord> {
    let value: Value = serde_json::from_slice(&persistence::read_bounded(path, MAX_RECORD)?)
        .map_err(|e| e.to_string())?;
    let record = match value["schema_version"].as_u64() {
        Some(1) => CatalogRecord::Legacy(serde_json::from_value(value).map_err(|e| e.to_string())?),
        Some(2) => {
            CatalogRecord::Logical(serde_json::from_value(value).map_err(|e| e.to_string())?)
        }
        _ => return Err("Unsupported creation reservation schema".into()),
    };
    record.validate()?;
    Ok(record)
}
fn sync(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())
}
impl WorldManager {
    pub(super) fn creation_path(&self, key: &str) -> Result<PathBuf> {
        workers::token(key)?;
        Ok(self
            .build_root
            .join("forks")
            .join(format!("{}.json", workers::digest(&json!(key))?)))
    }
    pub(super) fn read_creation(&self, key: &str) -> Result<CatalogRecord> {
        let record = read_record(&self.creation_path(key)?)?;
        if record.key() != key {
            return Err("Creation request key mismatch".into());
        }
        Ok(record)
    }
    // Read one bounded payload at a time. Retain only compact identities;
    // recovery rereads selected payloads after validating the whole catalog.
    fn creation_catalog(&self) -> Result<Vec<CatalogIdentity>> {
        let dir = self.build_root.join("forks");
        if !dir.try_exists().map_err(|e| e.to_string())? {
            if self
                .worlds
                .values()
                .any(|w| w.fork.is_some() || w.creation.is_some())
            {
                return Err("Created worlds have no reservation catalog".into());
            }
            return Ok(vec![]);
        }
        persistence::existing_dir(&dir)?;
        let mut records = Vec::new();
        let mut children = BTreeSet::new();
        for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("context" | "materialized")
            ) && !path
                .with_extension("json")
                .try_exists()
                .map_err(|e| e.to_string())?
            {
                return Err("Creation evidence has no durable reservation".into());
            }
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if records.len() >= MAX_RESERVATIONS {
                return Err("Creation reservation catalog limit".into());
            }
            let record = read_record(&path)?;
            let identity = record.identity()?;
            if path != self.creation_path(record.key())? || !children.insert(record.id().to_owned())
            {
                return Err("Conflicting creation reservation catalog".into());
            }
            for prior in &records {
                if identity.conflicts(prior)? {
                    return Err("Conflicting creation destinations".into());
                }
            }
            if let Some(world) = self.worlds.get(record.id()) {
                record.matches_world(world)?;
                self.creation_context(world)?;
            }
            records.push(identity);
        }
        for (id, world) in &self.worlds {
            if (world.fork.is_some() || world.creation.is_some()) && !children.contains(id) {
                return Err("Created world has no durable reservation".into());
            }
        }
        Ok(records)
    }
    pub(super) fn save_creation(&self, record: &CatalogRecord) -> Result<()> {
        self.ensure_starting_allowed()?;
        record.validate()?;
        let value = record.value()?;
        let size = serde_json::to_vec(&value).map_err(|e| e.to_string())?.len() as u64;
        if size > MAX_RECORD {
            return Err("Creation reservation exceeds size limit".into());
        }
        let records = self.creation_catalog()?;
        if records.len() >= MAX_RESERVATIONS {
            return Err("Creation reservation catalog limit".into());
        }
        let identity = record.identity()?;
        for prior in &records {
            if prior.key == record.key() || prior.conflicts(&identity)? {
                return Err("Creation destination or request already reserved".into());
            }
        }
        let path = self.creation_path(record.key())?;
        persistence::private_dir(path.parent().unwrap())?;
        sync(&self.build_root)?;
        persistence::atomic_json(&path, &value)
    }
    fn fence_creation(
        &self,
        record: &CatalogRecord,
        world: &World,
        fault: ForkFault,
    ) -> Result<()> {
        self.creation_context(world)?;
        let dir = self.build_root.join(record.id());
        sync(&dir.join("world.json"))?;
        fault
            .sync_child_directory(&dir)
            .map_err(|e| e.to_string())?;
        sync(&self.build_root)?;
        let marker = self
            .creation_path(record.key())?
            .with_extension("materialized");
        let identity = record.marker()?;
        if marker.try_exists().map_err(|e| e.to_string())? {
            let saved: Value =
                serde_json::from_slice(&persistence::read_bounded(&marker, MAX_RECORD)?)
                    .map_err(|e| e.to_string())?;
            if saved != identity {
                return Err("Creation materialization identity changed".into());
            }
            sync(&marker)?;
            sync(marker.parent().unwrap())?;
        } else {
            if world.generation != 0
                || world.state != "created"
                || !world.admission.records.is_empty()
                || world.admission.external_pending.is_some()
                || world.admission.external_head.is_some()
                || world.fork.as_ref().is_some_and(|f| f.ready)
                || world
                    .creation
                    .as_ref()
                    .is_some_and(|s| s.context_id.is_some() || s.confirmed)
                || world.persistence.restore_requested.is_some()
                || world.persistence.restored_from.is_some()
            {
                return Err("Progressed creation has no materialization fence".into());
            }
            persistence::atomic_json(&marker, &identity)?;
        }
        Ok(())
    }
    pub(super) fn materialize_creation(
        &mut self,
        record: &CatalogRecord,
        fault: ForkFault,
    ) -> Result<()> {
        let path = self.creation_path(record.key())?;
        for path in [
            path.as_path(),
            path.parent().unwrap(),
            self.build_root.as_path(),
        ] {
            sync(path)?;
        }
        record.validate()?;
        if let Some(world) = self.worlds.get(record.id()) {
            record.matches_world(world)?;
            return self.fence_creation(record, world, fault);
        }
        let dir = self.build_root.join(record.id());
        let world_path = dir.join("world.json");
        if world_path.try_exists().map_err(|e| e.to_string())? {
            let world: World =
                serde_json::from_slice(&persistence::read_bounded(&world_path, MAX_RECORD)?)
                    .map_err(|e| e.to_string())?;
            record.matches_world(&world)?;
            self.fence_creation(record, &world, fault)?;
            self.worlds.insert(record.id().to_owned(), world);
            return Ok(());
        }
        if ["materialized", "context"]
            .into_iter()
            .try_fold(false, |exists, extension| {
                self.creation_path(record.key())?
                    .with_extension(extension)
                    .try_exists()
                    .map(|present| exists || present)
                    .map_err(|e| e.to_string())
            })?
        {
            return Err("Materialized creation has lost its world control record".into());
        }
        if dir.try_exists().map_err(|e| e.to_string())? {
            for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
                if entry.map_err(|e| e.to_string())?.file_name() != "world.json.tmp" {
                    return Err("Creation progress evidence without world control record".into());
                }
            }
        }
        let pin = self.validate_world_definition(record.definition())?;
        let mut world = self.new_world(record.definition().clone(), &pin)?;
        record.initialize(&mut world)?;
        persistence::private_dir(&dir)?;
        sync(&self.build_root)?;
        persistence::persist_with_directory_sync(
            &self.build_root,
            record.id(),
            &mut world,
            |dir| fault.sync_child_directory(dir),
        )?;
        if matches!(fault, ForkFault::AfterChildRecord) {
            return Err("Injected failure before materialization fence".into());
        }
        self.fence_creation(record, &world, fault)?;
        self.worlds.insert(record.id().to_owned(), world);
        Ok(())
    }
    pub(super) fn recover_creations(&mut self) -> Result<()> {
        let records = self.creation_catalog()?;
        for identity in records {
            let record = self.read_creation(&identity.key)?;
            if record.id() != identity.id {
                return Err("Creation reservation changed during recovery".into());
            }
            self.materialize_creation(&record, ForkFault::None)?;
        }
        Ok(())
    }
    pub fn reserve_creation(&mut self, request: CreationRequest) -> Result<CreationReservation> {
        self.reserve_creation_with_fault(request, ForkFault::None)
    }
    pub fn reserve_creation_with_fault(
        &mut self,
        request: CreationRequest,
        fault: ForkFault,
    ) -> Result<CreationReservation> {
        workers::token(&request.request_key)?;
        request.destination.validate()?;
        self.validate_world_definition(&request.definition)?;
        if request.definition.external_publication.is_none() {
            return Err("Logical creation requires hosted publication".into());
        }
        boundary::validate_binding(&PublicationBinding {
            context: request.binding.clone(),
            parent_receipt_sha256: None,
        })?;
        let id = crate::bounded::owner_identity();
        let origin = if let Some(fork) = request.fork {
            if fork.request_key != request.request_key
                || fork.destination != request.destination.scope()
                || serde_json::to_value(&fork.definition).map_err(|e| e.to_string())?
                    != serde_json::to_value(&request.definition).map_err(|e| e.to_string())?
            {
                return Err("Logical fork request differs from destination/definition".into());
            }
            Origin::Fork {
                record: Box::new(self.checked_fork_record(*fork, id.clone())?),
            }
        } else {
            Origin::Fresh
        };
        let digest = request_digest(
            &request.request_key,
            &request.destination,
            &request.definition,
            &request.binding,
            &origin,
        )?;
        let path = self.creation_path(&request.request_key)?;
        let record = if path.try_exists().map_err(|e| e.to_string())? {
            let prior = self.read_creation(&request.request_key)?;
            let CatalogRecord::Logical(logical) = &prior else {
                return Err("Request key already binds a legacy fork".into());
            };
            if logical.reservation.request_sha256 != digest {
                return Err("Creation request key identifies different contents".into());
            }
            prior
        } else {
            let record = CatalogRecord::Logical(Box::new(LogicalRecord {
                schema_version: 2,
                reservation: CreationReservation {
                    schema_version: 2,
                    request_key: request.request_key,
                    request_sha256: digest,
                    world_id: id,
                    destination: request.destination,
                    binding_sha256: workers::digest(&request.binding)?,
                    kind: if matches!(origin, Origin::Fresh) {
                        "fresh"
                    } else {
                        "fork"
                    }
                    .into(),
                },
                definition: request.definition,
                binding: request.binding,
                origin,
            }));
            self.save_creation(&record)?;
            record
        };
        if matches!(fault, ForkFault::AfterReservation) {
            return Err("Injected failure after durable creation reservation".into());
        }
        self.materialize_creation(&record, fault)?;
        let CatalogRecord::Logical(record) = record else {
            unreachable!()
        };
        Ok(record.reservation)
    }
    pub fn resolve_creation(
        &mut self,
        destination: &LogicalDestination,
    ) -> Result<ResolvedCreation> {
        self.lookup_creation(destination)?
            .ok_or_else(|| "Unknown logical destination".into())
    }
    /// Typed absence for host preflight. Retained reservations still reconcile
    /// their materialization; corruption is an error, never reported as absence.
    pub fn lookup_creation(
        &mut self,
        destination: &LogicalDestination,
    ) -> Result<Option<ResolvedCreation>> {
        destination.validate()?;
        let identity =
            self.creation_catalog()?
                .into_iter()
                .find(|record| match &record.destination {
                    DestinationIdentity::Logical(value) => value == destination,
                    DestinationIdentity::Legacy { .. } => false,
                });
        let Some(identity) = identity else {
            return Ok(None);
        };
        let record = self.read_creation(&identity.key)?;
        self.materialize_creation(&record, ForkFault::None)?;
        let CatalogRecord::Logical(record) = record else {
            unreachable!()
        };
        let state = self.worlds[&record.reservation.world_id]
            .creation
            .as_ref()
            .ok_or("Missing creation state")?;
        let fork = match record.origin {
            Origin::Fresh => None,
            Origin::Fork { record } => Some(record.reservation),
        };
        Ok(Some(ResolvedCreation {
            reservation: record.reservation,
            definition: record.definition,
            binding: record.binding,
            context_id: self.creation_context(&self.worlds[&state.reservation.world_id])?,
            context_confirmed: state.confirmed,
            fork,
        }))
    }
    /// Candidate lives in the same reservation catalog before world readiness.
    /// This also checks that a ready world has not lost its durable candidate.
    fn creation_context(&self, world: &World) -> Result<Option<String>> {
        let Some(state) = &world.creation else {
            return Ok(None);
        };
        let path = self
            .creation_path(&state.reservation.request_key)?
            .with_extension("context");
        if !path.try_exists().map_err(|e| e.to_string())? {
            if state.context_id.is_some() || state.confirmed {
                return Err("Logical world has lost its context candidate".into());
            }
            return Ok(None);
        }
        let candidate: ContextCandidate =
            serde_json::from_slice(&persistence::read_bounded(&path, 16 * 1024)?)
                .map_err(|e| e.to_string())?;
        if candidate.schema_version != 1
            || candidate.reservation != state.reservation
            || !context_identity(&candidate.context_id)
            || state
                .context_id
                .as_ref()
                .is_some_and(|id| *id != candidate.context_id)
        {
            return Err("Logical context candidate changed".into());
        }
        Ok(Some(candidate.context_id))
    }
    /// Trusted host acknowledgment after exact storage-owned descriptor readback.
    /// A failed write retains the candidate and blocks activation until retried.
    pub fn confirm_creation_context(
        &mut self,
        reservation: CreationReservation,
        context_id: String,
    ) -> Result<ResolvedCreation> {
        if !context_identity(&context_id) {
            return Err("Invalid context identity".into());
        }
        let current = self.resolve_creation(&reservation.destination)?;
        if current.reservation != reservation {
            return Err("Creation reservation changed".into());
        }
        if current
            .context_id
            .as_ref()
            .is_some_and(|id| *id != context_id)
        {
            return Err("Creation context identity changed".into());
        }
        let path = self
            .creation_path(&reservation.request_key)?
            .with_extension("context");
        if current.context_id.is_none() {
            persistence::atomic_json(
                &path,
                &serde_json::to_value(ContextCandidate {
                    schema_version: 1,
                    reservation: reservation.clone(),
                    context_id: context_id.clone(),
                })
                .map_err(|e| e.to_string())?,
            )?;
        }
        sync(&path)?;
        sync(path.parent().unwrap())?;
        if !current.context_confirmed {
            let world = self.worlds.get_mut(&reservation.world_id).unwrap();
            let state = world.creation.as_mut().unwrap();
            state.context_id = Some(context_id);
            state.confirmed = true;
            world.persistence_dirty = true;
            if let Err(error) = persistence::persist(&self.build_root, &reservation.world_id, world)
            {
                world.creation.as_mut().unwrap().confirmed = false;
                world.persistence_dirty = true;
                return Err(error);
            }
        }
        self.resolve_creation(&reservation.destination)
    }
}
