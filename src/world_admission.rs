//! Owner-serialized typed admission, durable boundaries and generic effect fences.
//! No external effect is executed here, and a replay never grants dispatch authority.
use super::{persist, timestamp, workers, ScenarioChange, World, WorldManager};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

type Result<T> = std::result::Result<T, String>;
const MAX_ADMISSIONS: usize = 4096;
const MAX_EFFECTS: usize = 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectRequest {
    pub key: String,
    pub phase: String,
    #[serde(default)]
    pub reservation_key: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmitInputs {
    pub id: String,
    pub expected_generation: u64,
    pub expected_revision: u64,
    pub admission_key: String,
    pub changes: Vec<ScenarioChange>,
    #[serde(default)]
    pub worker_id: Option<String>,
    #[serde(default)]
    pub effect: Option<EffectRequest>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionQuery {
    pub id: String,
    pub generation: u64,
    pub admission_key: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadBatch {
    pub id: String,
    pub expected_generation: u64,
    pub expected_revision: u64,
    #[serde(default)]
    pub worker_id: Option<String>,
    pub queries: Vec<ReadQuery>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadQuery {
    pub predicate: String,
    #[serde(default = "default_rows")]
    pub max_rows: usize,
    #[serde(default)]
    pub continuation: Option<Value>,
}
fn default_rows() -> usize {
    100
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Effect {
    world_id: String,
    generation: u64,
    worker_id: Option<String>,
    reservation_key: String,
    state: String,
    settlement_key: Option<String>,
    revision: u64,
}
pub(super) type Effects = BTreeMap<String, Effect>;
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdmissionState {
    records: BTreeMap<String, Record>,
    pub effects: Effects,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    generation: u64,
    admission_key: String,
    request_sha256: String,
    effect: Option<EffectRequest>,
    state: String,
    applied_revision: Option<u64>,
    receipt: Option<Value>,
    error: Option<String>,
    publication: String,
    at_unix_ms: u128,
}
impl Record {
    fn result(&self, id: &str, replayed: bool, authority: bool) -> Value {
        json!({"schema_version":1,"id":id,"generation":self.generation,"admission_key":self.admission_key,
            "state":self.state,"applied_revision":self.applied_revision,"receipt":self.receipt,"error":self.error,
            "publication":self.publication,"replayed":replayed,
            "effect_authorized":authority && !replayed && self.state=="durable" && self.effect.as_ref().is_some_and(|e|e.phase=="reserve")})
    }
}
fn record_key(generation: u64, key: &str) -> String {
    format!("{generation}/{key}")
}
fn fence(world: &World, generation: u64, revision: u64, worker_id: Option<&str>) -> Result<()> {
    if world.state != "running" || world.pending.is_some() || world.generation != generation {
        return Err("Expected running generation mismatch; request not applied".into());
    }
    let instance = world
        .instance
        .as_ref()
        .ok_or("World has no live instance")?;
    if instance.backend.revision() != revision {
        return Err("Expected revision mismatch; request not applied".into());
    }
    workers::check_worker(world, worker_id)
}
fn validate_effect(world: &World, request: &AdmitInputs) -> Result<()> {
    let Some(effect) = &request.effect else {
        return Ok(());
    };
    workers::token(&effect.key)?;
    match effect.phase.as_str() {
        "reserve" => {
            if effect.reservation_key.is_some() {
                return Err("Reservation must not supply a reservation_key".into());
            }
            if world.admission.effects.contains_key(&effect.key) {
                return Err(
                    "Effect already reserved or settled; automatic redispatch is forbidden".into(),
                );
            }
            if world.admission.effects.len() >= MAX_EFFECTS {
                return Err("World effect retention limit reached".into());
            }
        }
        "settle" => {
            let key = effect
                .reservation_key
                .as_ref()
                .ok_or("Settlement requires reservation_key")?;
            let prior = world
                .admission
                .effects
                .get(&effect.key)
                .ok_or("Effect has no reservation")?;
            if prior.state != "reserved"
                || prior.world_id != request.id
                || prior.generation != request.expected_generation
                || prior.worker_id != request.worker_id
                || &prior.reservation_key != key
            {
                return Err(
                    "Effect settlement fence mismatch; duplicate/late settlement is forbidden"
                        .into(),
                );
            }
        }
        _ => return Err("Effect phase must be reserve or settle".into()),
    }
    Ok(())
}
fn apply_effect(world: &mut World, request: &AdmitInputs, revision: u64) {
    if let Some(effect) = &request.effect {
        if effect.phase == "reserve" {
            world.admission.effects.insert(
                effect.key.clone(),
                Effect {
                    world_id: request.id.clone(),
                    generation: request.expected_generation,
                    worker_id: request.worker_id.clone(),
                    reservation_key: request.admission_key.clone(),
                    state: "reserved".into(),
                    settlement_key: None,
                    revision,
                },
            );
        } else if let Some(prior) = world.admission.effects.get_mut(&effect.key) {
            prior.state = "settled".into();
            prior.settlement_key = Some(request.admission_key.clone());
            prior.revision = revision;
        }
    }
}
pub(super) fn validate_effects(value: Option<&Value>) -> Result<Effects> {
    let effects: Effects = match value {
        None => BTreeMap::new(),
        Some(v) => {
            serde_json::from_value(v.clone()).map_err(|_| "Invalid checkpoint effect metadata")?
        }
    };
    if effects.len() > MAX_EFFECTS {
        return Err("Checkpoint effect metadata exceeds limit".into());
    }
    for (key, effect) in &effects {
        workers::token(key)?;
        workers::token(&effect.reservation_key)?;
        workers::token(&effect.world_id)?;
        if effect.generation == 0
            || effect.revision == 0
            || !matches!(
                effect.state.as_str(),
                "reserved" | "settled" | "unpublished" | "uncertain"
            )
        {
            return Err("Invalid checkpoint effect fence".into());
        }
    }
    Ok(effects)
}
pub(super) fn recover(world: &mut World) {
    for record in world.admission.records.values_mut() {
        if matches!(record.state.as_str(), "pending" | "applied") {
            record.state = "uncertain".into();
            record.error =
                Some("Owner exited before a durable admission outcome was recorded".into());
            record.publication = "uncertain".into();
            record.receipt = None;
            world.persistence_dirty = true;
        }
    }
}
impl WorldManager {
    pub fn read_batch(&mut self, request: ReadBatch) -> Result<Value> {
        self.ensure_starting_allowed()?;
        self.status_with(&request.id, true)?;
        let world = self.worlds.get_mut(&request.id).ok_or("Unknown world")?;
        fence(
            world,
            request.expected_generation,
            request.expected_revision,
            request.worker_id.as_deref(),
        )?;
        if request.queries.is_empty()
            || request.queries.len() > 16
            || request
                .queries
                .iter()
                .any(|q| q.max_rows == 0 || q.max_rows > 1000)
            || request.queries.iter().map(|q| q.max_rows).sum::<usize>() > 1000
        {
            return Err("Read batch requires 1–16 queries and at most 1000 requested rows".into());
        }
        let mut results = Vec::new();
        let mut bytes = 0;
        for query in request.queries {
            let page=world.instance.as_mut().unwrap().execute("query_rows",&json!({"predicate":query.predicate,"max_rows":query.max_rows,"continuation":query.continuation,"max_bytes":crate::MAX_QUERY_BYTES}))?;
            bytes += serde_json::to_vec(&page).map_err(|e| e.to_string())?.len();
            if bytes > crate::MAX_QUERY_BYTES {
                return Err("Read batch exceeds 4 MiB; reduce page sizes".into());
            }
            results.push(page);
        }
        let result = json!({"schema_version":1,"id":request.id,"generation":world.generation,"revision":request.expected_revision,"results":results});
        if serde_json::to_vec(&result)
            .map_err(|e| e.to_string())?
            .len()
            > crate::MAX_QUERY_BYTES
        {
            return Err("Read batch exceeds 4 MiB; reduce page sizes".into());
        }
        Ok(result)
    }
    pub fn admission_status(&mut self, request: AdmissionQuery) -> Result<Value> {
        let world = self.worlds.get(&request.id).ok_or("Unknown world")?;
        let record = world
            .admission
            .records
            .get(&record_key(request.generation, &request.admission_key))
            .ok_or("Unknown admission")?;
        Ok(record.result(&request.id, true, false))
    }
    pub fn admit_inputs(&mut self, request: AdmitInputs) -> Result<Value> {
        workers::token(&request.admission_key)?;
        if request.changes.len() > 1000
            || serde_json::to_vec(&request)
                .map_err(|e| e.to_string())?
                .len()
                > 256 * 1024
        {
            return Err("Admission exceeds 1000 changes or 256 KiB".into());
        }
        let key = record_key(request.expected_generation, &request.admission_key);
        let hash = workers::digest(&request)?;
        let world = self.worlds.get(&request.id).ok_or("Unknown world")?;
        if let Some(record) = world.admission.records.get(&key) {
            if record.request_sha256 != hash {
                return Err(
                    "Admission key already identifies different contents; request not applied"
                        .into(),
                );
            }
            return Ok(record.result(&request.id, true, false));
        }
        let mut record = Record {
            generation: request.expected_generation,
            admission_key: request.admission_key.clone(),
            request_sha256: hash,
            effect: request.effect.clone(),
            state: "not_applied".into(),
            applied_revision: None,
            receipt: None,
            error: None,
            publication: "not_attempted".into(),
            at_unix_ms: timestamp(),
        };
        let prepared = (|| {
            self.ensure_starting_allowed()?;
            self.status_with(&request.id, true)?;
            let world = self.worlds.get(&request.id).ok_or("Unknown world")?;
            fence(
                world,
                request.expected_generation,
                request.expected_revision,
                request.worker_id.as_deref(),
            )?;
            if world.admission.records.len() >= MAX_ADMISSIONS {
                return Err("World admission retention limit reached".into());
            }
            validate_effect(world, &request)?;
            let instance = world.instance.as_ref().unwrap();
            // Reject unsupported checkpoint programs before a mutation. The
            // later publication still has an independent failure outcome.
            instance.checkpoint_identity()?;
            instance.backend.checkpoint_bytes(Value::Null)?;
            instance.prepare_admission_changes(
                &serde_json::to_value(&request.changes).map_err(|e| e.to_string())?,
            )
        })();
        let changes = match prepared {
            Ok(c) => c,
            Err(e) => {
                record.error = Some(e);
                return Ok(record.result(&request.id, false, false));
            }
        };
        let world = self.worlds.get_mut(&request.id).unwrap();
        record.state = "pending".into();
        world.admission.records.insert(key.clone(), record.clone());
        world.persistence_dirty = true;
        if let Err(error) = persist(&self.build_root, &request.id, world) {
            record.state = "not_applied".into();
            record.error = Some(error);
            world.admission.records.insert(key, record.clone());
            world.persistence_dirty = true;
            return Ok(record.result(&request.id, false, false));
        }
        match world
            .instance
            .as_mut()
            .unwrap()
            .backend
            .apply_without_deltas(&changes)
        {
            Ok(_) => {
                let revision = world.instance.as_ref().unwrap().backend.revision();
                record.applied_revision = Some(revision);
                record.state = "applied".into();
                apply_effect(world, &request, revision);
                world.admission.records.insert(key.clone(), record.clone());
                world.persistence_dirty = true;
            }
            Err(_) => {
                record.state = "uncertain".into();
                record.error =
                    Some("Native input acknowledgment failed; reconcile explicitly".into());
                world.admission.records.insert(key, record.clone());
                world.persistence_dirty = true;
                let _ = persist(&self.build_root, &request.id, world);
                return Ok(record.result(&request.id, false, false));
            }
        }
        match self.checkpoint(&request.id) {
            Ok(receipt) => {
                record.state = "durable".into();
                record.publication = "published".into();
                record.receipt = Some(receipt);
            }
            Err(error) => {
                record.state = "applied_but_unpublished".into();
                record.publication = "uncertain".into();
                record.error = Some(error);
                if let Some(effect) = &request.effect {
                    if let Some(effect) = self
                        .worlds
                        .get_mut(&request.id)
                        .unwrap()
                        .admission
                        .effects
                        .get_mut(&effect.key)
                    {
                        effect.state = "unpublished".into();
                    }
                }
            }
        }
        let world = self.worlds.get_mut(&request.id).unwrap();
        workers::poll(world);
        let authority = workers::check_worker(world, request.worker_id.as_deref()).is_ok()
            && !self
                .shutdown
                .stopped
                .load(std::sync::atomic::Ordering::SeqCst);
        world.admission.records.insert(key.clone(), record.clone());
        world.persistence_dirty = true;
        if let Err(error) = persist(&self.build_root, &request.id, world) {
            record.state = "uncertain".into();
            record.publication = "uncertain".into();
            record.error = Some(error);
            record.receipt = None;
            if let Some(effect) = request
                .effect
                .as_ref()
                .and_then(|e| world.admission.effects.get_mut(&e.key))
            {
                effect.state = "uncertain".into();
            }
            world.admission.records.insert(key, record.clone());
            world.persistence_dirty = true;
        }
        Ok(record.result(&request.id, false, authority))
    }
}
