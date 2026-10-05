//! Program state and semantic operations, independent of MCP and connections.
use crate::composition::CompositionResolution;
use crate::registry::{
    GitProvenance, ProcessorDefinition, ProcessorReference, ProcessorRegistry, ProcessorVersion,
};
use crate::{AgentProgram, Backend, LoweringOptions, Operation, Schema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// One relation addressable through a world's public tools. `physical` is the
/// generated native relation name; every other field is the public contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicRelation {
    pub name: String,
    pub input: bool,
    pub fields: Vec<String>,
    pub physical: String,
}

/// Public relations of one registered definition, without a live instance.
/// A program without an interface exposes every declared schema; a program with
/// an interface exposes its interface inputs and outputs; a composition exposes
/// its resolution inputs and outputs. Inputs precede outputs, each sorted by name.
pub fn public_relations(record: &ProcessorVersion) -> Result<Vec<PublicRelation>, String> {
    match &record.definition {
        ProcessorDefinition::Program(program) => {
            let schemas: BTreeMap<String, Schema> =
                serde_json::from_value(program.schemas.clone()).map_err(|e| e.to_string())?;
            let relation = |name: &str| -> Result<PublicRelation, String> {
                let schema = schemas
                    .get(name)
                    .ok_or_else(|| format!("Interface relation {name} is not declared"))?;
                Ok(PublicRelation {
                    name: name.to_string(),
                    input: schema.input,
                    fields: schema.fields.clone(),
                    physical: name.to_string(),
                })
            };
            let mut names: Vec<&str> = match &program.interface {
                Some(interface) => interface
                    .inputs
                    .iter()
                    .chain(&interface.outputs)
                    .map(String::as_str)
                    .collect(),
                None => schemas.keys().map(String::as_str).collect(),
            };
            names.sort_by_key(|name| (!schemas.get(*name).is_some_and(|s| s.input), *name));
            names.iter().map(|name| relation(name)).collect()
        }
        ProcessorDefinition::Composition(_) => {
            let resolution = record
                .composition
                .as_ref()
                .ok_or("Composition record lacks its resolution")?;
            let mut relations = Vec::new();
            for (input, ports) in [(true, &resolution.inputs), (false, &resolution.outputs)] {
                for (public, physical) in ports {
                    let fields = resolution
                        .relations
                        .get(physical)
                        .and_then(|r| r.get("fields"))
                        .cloned()
                        .ok_or_else(|| format!("Composition relation {physical} lacks fields"))?;
                    relations.push(PublicRelation {
                        name: public.clone(),
                        input,
                        fields: serde_json::from_value(fields).map_err(|e| e.to_string())?,
                        physical: physical.clone(),
                    });
                }
            }
            Ok(relations)
        }
    }
}

/// Identity of the actual pure program build, independent of native execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProgramIdentity {
    pub processor: ProcessorReference,
    pub dependencies: BTreeMap<String, ProcessorReference>,
    pub public_relations: Vec<PublicRelation>,
    pub lowering_version: u32,
    pub source_sha256: String,
}

/// External-publication policy validates checkpoint eligibility before activation.
pub(crate) fn validate_checkpoint_program(
    registry: &ProcessorRegistry,
    record: &ProcessorVersion,
) -> Result<(), String> {
    let prepared = PreparedProgram::new(registry, record, 2)?;
    if prepared
        .source
        .lines()
        .any(|line| line.trim_start().starts_with("import "))
    {
        return Err("External publication does not support imported native operators".into());
    }
    Ok(())
}

/// Validate before an imported receipt changes hosted admission state.
pub(crate) fn validate_pinned_checkpoint(
    registry: &ProcessorRegistry,
    bytes: &[u8],
    identity: &ProgramIdentity,
) -> Result<(), String> {
    prepare_pinned_checkpoint(registry, bytes, identity).map(|_| ())
}
fn prepare_pinned_checkpoint(
    registry: &ProcessorRegistry,
    bytes: &[u8],
    identity: &ProgramIdentity,
) -> Result<(ProcessorVersion, PreparedProgram), String> {
    registry.ensure_active(&identity.processor.processor_id)?;
    let record = registry.get(
        &identity.processor.processor_id,
        Some(&identity.processor.version),
    )?;
    let prepared = PreparedProgram::new(registry, &record, identity.lowering_version)?;
    let (_, state) = crate::checkpoint::inspect(bytes)?;
    if &prepared.identity != identity
        || state.source != prepared.source
        || state.schemas != prepared.schemas
        || state.lowering_version != identity.lowering_version
    {
        return Err(
            "Checkpoint does not match pinned program, dependencies, public interfaces or lowering"
                .into(),
        );
    }
    Ok((record, prepared))
}

/// The same lowering and public admission mapping serve install and restore.
/// Preparing this plan validates pins/dependencies without starting a compiler.
struct PreparedProgram {
    source: String,
    schemas: BTreeMap<String, Schema>,
    interface: Option<PublicInterface>,
    composition: Option<CompositionResolution>,
    identity: ProgramIdentity,
}
impl PreparedProgram {
    fn new(
        registry: &ProcessorRegistry,
        record: &ProcessorVersion,
        version: u32,
    ) -> Result<Self, String> {
        let options = LoweringOptions::for_version(version)?;
        let (source, schemas, interface, composition) = match &record.definition {
            ProcessorDefinition::Composition(definition) => {
                let compiled =
                    registry.compile_composition_versioned(&definition.composition, version)?;
                let interface = PublicInterface {
                    inputs: compiled.resolution.inputs.clone(),
                    outputs: compiled.resolution.outputs.clone(),
                };
                (
                    compiled.source,
                    compiled.schemas,
                    Some(interface),
                    Some(compiled.resolution),
                )
            }
            ProcessorDefinition::Program(definition) => {
                if definition.operation.is_some() {
                    return Err(
                        "Registered-operation programs do not support managed checkpoints".into(),
                    );
                }
                let schemas = serde_json::from_value(definition.schemas.clone())
                    .map_err(|e| e.to_string())?;
                let exports = definition
                    .interface
                    .as_ref()
                    .map(|i| i.outputs.iter().cloned().collect());
                let source = crate::lower_with_options(
                    &definition.rules,
                    &schemas,
                    &definition.operators,
                    &options,
                    exports.as_ref(),
                )?;
                let interface = definition
                    .interface
                    .as_ref()
                    .map(PublicInterface::from_program);
                (source, schemas, interface, None)
            }
        };
        let mut actual = record.clone();
        actual.composition = composition.clone();
        use sha2::{Digest, Sha256};
        let identity = ProgramIdentity {
            processor: ProcessorReference {
                processor_id: record.processor_id.clone(),
                version: record.version.clone(),
            },
            dependencies: composition
                .as_ref()
                .map(|c| c.dependencies.clone())
                .unwrap_or_default(),
            public_relations: public_relations(&actual)?,
            lowering_version: version,
            source_sha256: format!("{:x}", Sha256::digest(source.as_bytes())),
        };
        Ok(Self {
            source,
            schemas,
            interface,
            composition,
            identity,
        })
    }
}

/// Continuation for paged reads of retained input facts. Output continuations
/// are the native-bound [`crate::QueryCursor`]; both are opaque to callers.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputCursor {
    kind: String,
    revision: u64,
    predicate: String,
    offset: usize,
}

/// One semantic owner whose caller controls its lifetime independently of connections.
pub struct ProgramInstance {
    pub(super) backend: Backend,
    pub(crate) operations: BTreeMap<String, Operation>,
    agent: Option<AgentProgram>,
    pub(crate) registry: Option<ProcessorRegistry>,
    pub(crate) instance_id: Option<String>,
    processor: Option<Value>,
    record: Option<ProcessorVersion>,
    interface: Option<PublicInterface>,
    composition: Option<CompositionResolution>,
}

impl ProgramInstance {
    pub fn new(
        backend: Backend,
        operations: BTreeMap<String, Operation>,
        registry: Option<ProcessorRegistry>,
        instance_id: Option<String>,
    ) -> Self {
        Self {
            backend,
            operations,
            registry,
            instance_id,
            agent: None,
            processor: None,
            record: None,
            interface: None,
            composition: None,
        }
    }

    /// Lowering version (1 or 2) for the next `processor_install` or
    /// `lemmalog_install_rules`. It changes only the generated text this
    /// instance builds; the registered record and its hash are untouched.
    pub fn set_lowering_version(&mut self, version: u32) -> Result<(), String> {
        self.backend.set_lowering_version(version)
    }

    /// Execute one semantic operation at the last completed transaction.
    ///
    /// Operation names and JSON argument/result shapes match the documented
    /// program operations, without a JSON-RPC envelope or connection lifecycle.
    /// This is the same admission path used by MCP: immutable pins, exported
    /// ports and registered request ownership cannot be bypassed here.
    /// An error never authorizes automatic retry of an uncertain mutation.
    pub fn execute(&mut self, name: &str, a: &Value) -> Result<Value, String> {
        if name == "instance_info" && self.instance_id.is_some() {
            let live = self.backend.health() == "ready";
            return Ok(json!({
                "instance_id":self.instance_id,"health":self.backend.health(),
                "processor":self.processor,"composition":self.composition,
                "revision":live.then(|| self.backend.revision()),
                "program_version":live.then_some(self.backend.version),
                "build":live.then(|| self.backend.build_identity()),
                "source_sha256":live.then(|| self.backend.source_sha256()),
                "lowering_version":live.then(|| self.backend.lowering_version()),
            }));
        }
        if name.starts_with("processor_") && name != "processor_install" {
            return self.execute_registry(name, a);
        }
        if self.instance_id.is_some()
            && self.backend.health() == "failed"
            && name != "agent_request_status"
            && name != "agent_operations"
        {
            return Err("Instance runtime failed; reconcile uncertain work and explicitly create a new instance. Reconnect does not recover state.".into());
        }
        if self.processor.is_some()
            && matches!(
                name,
                "processor_install" | "lemmalog_install_rules" | "install_agent_program"
            )
        {
            return Err("Instance is pinned to an immutable processor version; create a new instance to select another version".into());
        }
        match name {
            "processor_install" => self.install_processor(a),
            "agent_operations" => Ok(
                json!({"operations":self.operations.iter().map(|(name,op)|json!({"name":name,"version":op.version,"description":op.description,"input":"string","output":"string"})).collect::<Vec<_>>()}),
            ),
            "install_agent_program" => {
                if !parse_operators(a)?.is_empty() {
                    return Err("Typed operators cannot be combined with a registered operation; install a separate pure program using lemmalog_install_rules or processor_install".into());
                }
                let name = string(a, "operation")?;
                let operation = self
                    .operations
                    .get(name)
                    .ok_or("Operation is not registered")?
                    .clone();
                let (agent, result) = AgentProgram::install(
                    &mut self.backend,
                    name,
                    operation,
                    string(a, "rules")?,
                    a["schemas"].clone(),
                )?;
                self.agent = Some(agent);
                Ok(result)
            }
            "submit_agent_input" => self
                .agent
                .as_mut()
                .ok_or("Install an agent program")?
                .submit(
                    &mut self.backend,
                    string(a, "entity")?,
                    a["revision"].as_i64().ok_or("Missing revision")?,
                    string(a, "payload")?,
                ),
            "claim_agent_request" => self
                .agent
                .as_mut()
                .ok_or("Install an agent program")?
                .claim(&mut self.backend, string(a, "request_id")?),
            "complete_agent_request" => self
                .agent
                .as_mut()
                .ok_or("Install an agent program")?
                .complete(
                    &mut self.backend,
                    string(a, "request_id")?,
                    string(a, "output")?,
                ),
            "agent_request_status" => self
                .agent
                .as_ref()
                .map(AgentProgram::status)
                .ok_or("Install an agent program".into()),
            "lemmalog_install_rules" if self.agent.is_some() => {
                Err("Create a new instance to replace a registered agent program".into())
            }
            "lemmalog_install_rules" => self.backend.install_with_operators(
                string(a, "rules")?,
                a["schemas"].clone(),
                &parse_operators(a)?,
            ),
            "apply_changes" => {
                if self.agent.is_some()
                    && a["changes"].as_array().is_some_and(|changes| {
                        changes.iter().any(|c| {
                            c["predicate"]
                                .as_str()
                                .is_some_and(|p| p.starts_with("agent_"))
                        })
                    })
                {
                    return Err(
                        "Registered operation relations must use the operation tools".into(),
                    );
                }
                let mut result = if let Some(interface) = &self.interface {
                    let changes = interface.changes(&a["changes"])?;
                    let mut result = self.backend.apply(&changes)?;
                    result["deltas"] =
                        json!(interface.outputs(result["deltas"].as_str().unwrap_or("")));
                    result
                } else {
                    self.backend.apply(&a["changes"])?
                };
                result["revision"] = json!(self.backend.revision());
                Ok(result)
            }
            "relations" => {
                let mut relations = Vec::new();
                for relation in self.public_relations()? {
                    let count = if relation.input {
                        self.backend
                            .export_inputs()?
                            .get(&relation.physical)
                            .map_or(0, |rows| rows.len() as u64)
                    } else {
                        self.backend.count_rows(&relation.physical)?
                    };
                    relations.push(json!({"name":relation.name,"input":relation.input,"fields":relation.fields,"count":count}));
                }
                Ok(json!({"revision":self.backend.revision(),"relations":relations}))
            }
            "query_rows" => self.query_rows(a),
            "program_source" => {
                if self.backend.health() != "ready" {
                    return Err(
                        "Program source is available from a healthy installed instance".into(),
                    );
                }
                Ok(
                    json!({"source":self.backend.program_source(),"source_sha256":self.backend.source_sha256()}),
                )
            }
            "lemmalog_query" => {
                let predicate = string(a, "predicate")?;
                if let Some(interface) = &self.interface {
                    let physical = interface
                        .outputs
                        .get(predicate)
                        .ok_or_else(|| format!("Unknown exported output {predicate}"))?;
                    let mut result = self.backend.query(physical)?;
                    result["rows"] =
                        json!(interface.outputs(result["rows"].as_str().unwrap_or("")));
                    Ok(result)
                } else {
                    self.backend.query(predicate)
                }
            }
            "lemmalog_why" => {
                let rule =
                    usize::try_from(a["rule"].as_u64().ok_or("Missing nonnegative rule index")?)
                        .map_err(|e| e.to_string())?;
                let mut result = self.backend.why(rule)?;
                if let Some(composition) = &self.composition {
                    result["origin"] = composition
                        .rules
                        .get(rule)
                        .ok_or("Unknown composition rule index")?
                        .clone();
                }
                Ok(result)
            }
            _ => Err("Unknown tool".into()),
        }
    }

    /// Public relations of the running program: the pinned record's contract
    /// when installed from the registry, otherwise every declared schema.
    pub fn public_relations(&self) -> Result<Vec<PublicRelation>, String> {
        if let Some(record) = &self.record {
            return public_relations(record);
        }
        if self.backend.health() != "ready" {
            return Err("Install a program first".into());
        }
        let mut relations: Vec<PublicRelation> = self
            .backend
            .schemas()
            .iter()
            .map(|(name, schema)| PublicRelation {
                name: name.clone(),
                input: schema.input,
                fields: schema.fields.clone(),
                physical: name.clone(),
            })
            .collect();
        relations.sort_by_key(|relation| (!relation.input, relation.name.clone()));
        Ok(relations)
    }

    /// Typed page of one public relation. Outputs stream through the bounded
    /// native reader; inputs page over retained facts. `total` is always the
    /// full row count at the reported revision.
    fn query_rows(&mut self, a: &Value) -> Result<Value, String> {
        let predicate = string(a, "predicate")?;
        let max_rows = match a.get("max_rows") {
            None | Some(Value::Null) => 500,
            Some(value) => usize::try_from(value.as_u64().ok_or("max_rows must be an integer")?)
                .map_err(|e| e.to_string())?,
        };
        if !(1..=crate::MAX_QUERY_ROWS).contains(&max_rows) {
            return Err(format!("max_rows must be in 1..={}", crate::MAX_QUERY_ROWS));
        }
        let max_bytes = match a.get("max_bytes") {
            None | Some(Value::Null) => crate::MAX_QUERY_BYTES,
            Some(value) => usize::try_from(value.as_u64().ok_or("max_bytes must be an integer")?)
                .map_err(|e| e.to_string())?,
        };
        if !(2..=crate::MAX_QUERY_BYTES).contains(&max_bytes) {
            return Err("Invalid query byte limit".into());
        }
        let relation = self
            .public_relations()?
            .into_iter()
            .find(|relation| relation.name == predicate)
            .ok_or_else(|| format!("Unknown public relation {predicate}"))?;
        let continuation = a.get("continuation").filter(|value| !value.is_null());
        if relation.input {
            if self.backend.health() != "ready" {
                return Err("Input query requires a healthy initialized runtime".into());
            }
            let offset = match continuation {
                Some(cursor) => {
                    let cursor: InputCursor =
                        serde_json::from_value(cursor.clone()).map_err(|e| e.to_string())?;
                    if cursor.kind != "input"
                        || cursor.revision != self.backend.revision()
                        || cursor.predicate != predicate
                    {
                        return Err(
                            "Continuation does not match the live owner, revision or query".into(),
                        );
                    }
                    cursor.offset
                }
                None => 0,
            };
            let all = self
                .backend
                .facts
                .iter()
                .filter(|((name, _), _)| name == &relation.physical)
                .map(|(_, row)| row);
            let total = all.clone().count();
            if offset > total {
                return Err("Continuation offset exceeds the selected relation".into());
            }
            let mut rows = Vec::new();
            let mut bytes = 2;
            for row in all.skip(offset).take(max_rows) {
                let size = serde_json::to_vec(row).map_err(|e| e.to_string())?.len()
                    + usize::from(!rows.is_empty());
                if bytes + size > max_bytes {
                    if rows.is_empty() {
                        return Err("Input row exceeds query byte limit".into());
                    }
                    break;
                }
                bytes += size;
                rows.push(row.clone());
            }
            let end = offset + rows.len();
            let complete = end >= total;
            let revision = self.backend.revision();
            return Ok(json!({
                "predicate":predicate,"revision":revision,"fields":relation.fields,"rows":rows,
                "total":total,"complete":complete,
                "continuation":(!complete).then(|| json!(InputCursor{kind:"input".into(),revision,predicate:predicate.into(),offset:end})),
            }));
        }
        let query = crate::BoundedQuery {
            filters: BTreeMap::new(),
            max_rows,
            max_bytes,
            continuation: continuation
                .map(|cursor| serde_json::from_value(cursor.clone()).map_err(|e| e.to_string()))
                .transpose()?,
        };
        let page = self
            .backend
            .query_typed_bounded(&relation.physical, &query)?;
        let total = self.backend.count_rows(&relation.physical)?;
        Ok(json!({
            "predicate":predicate,"revision":page.revision,"fields":relation.fields,"rows":page.rows,
            "total":total,"complete":!page.truncated,
            "continuation":page.continuation.map(|cursor| serde_json::to_value(cursor).unwrap_or(Value::Null)),
        }))
    }

    fn execute_registry(&self, name: &str, a: &Value) -> Result<Value, String> {
        let registry = self
            .registry
            .as_ref()
            .ok_or("Processor registry is not configured")?;
        if name == "processor_list" || name == "processor_search" {
            let limit = a
                .get("limit")
                .map(|value| value.as_u64().ok_or("limit must be an integer"))
                .transpose()?
                .unwrap_or(20);
            let limit = usize::try_from(limit).map_err(|e| e.to_string())?;
            let after = a.get("after").map(|_| string(a, "after")).transpose()?;
            let include_archived = a
                .get("include_archived")
                .map(|value| value.as_bool().ok_or("include_archived must be a boolean"))
                .transpose()?
                .unwrap_or(false);
            let page = if name == "processor_search" {
                registry.search(string(a, "query")?, limit, after, include_archived)?
            } else {
                registry.list(limit, after, include_archived)?
            };
            return serde_json::to_value(page).map_err(|e| e.to_string());
        }
        if name == "processor_archive" || name == "processor_restore" {
            let expected_revision = a["expected_revision"]
                .as_u64()
                .ok_or("Missing nonnegative expected_revision")?;
            let lifecycle = if name == "processor_archive" {
                registry.archive(
                    string(a, "processor_id")?,
                    string(a, "expected_version")?,
                    expected_revision,
                )?
            } else {
                registry.restore(
                    string(a, "processor_id")?,
                    string(a, "expected_version")?,
                    expected_revision,
                )?
            };
            return serde_json::to_value(lifecycle).map_err(|e| e.to_string());
        }
        let provenance = || -> Result<Option<GitProvenance>, String> {
            a.get("git_provenance")
                .filter(|v| !v.is_null())
                .map(|v| serde_json::from_value(v.clone()).map_err(|e| e.to_string()))
                .transpose()
        };
        let definition = || -> Result<ProcessorDefinition, String> {
            // Select the shape before deserialization so missing/unknown
            // fields remain visible instead of an opaque untagged-enum error.
            if a["definition"].get("composition").is_some() {
                serde_json::from_value(a["definition"].clone())
                    .map(ProcessorDefinition::Composition)
                    .map_err(|e| e.to_string())
            } else {
                serde_json::from_value(a["definition"].clone())
                    .map(ProcessorDefinition::Program)
                    .map_err(|e| e.to_string())
            }
        };
        let record = match name {
            "processor_create" => {
                registry.create_versioned(definition()?, provenance()?, lowering_version(a, 1)?)?
            }
            "processor_publish" => registry.publish_versioned(
                string(a, "processor_id")?,
                definition()?,
                string(a, "expected_version")?,
                provenance()?,
                lowering_version(a, 1)?,
            )?,
            "processor_fork" => registry.fork(
                string(a, "processor_id")?,
                string(a, "version")?,
                provenance()?,
            )?,
            "processor_get" => registry.get(
                string(a, "processor_id")?,
                a.get("version").map(|_| string(a, "version")).transpose()?,
            )?,
            _ => return Err("Unknown tool".into()),
        };
        serde_json::to_value(record).map_err(|e| e.to_string())
    }

    pub(crate) fn prepare_admission_changes(&self, changes: &Value) -> Result<Value, String> {
        if self.agent.is_some() {
            return Err("Durable input admission requires a pure program".into());
        }
        let changes = self
            .interface
            .as_ref()
            .map(|i| i.changes(changes))
            .unwrap_or_else(|| Ok(changes.clone()))?;
        for change in changes.as_array().ok_or("Expected changes array")? {
            if !matches!(change["op"].as_str(), Some("insert" | "delete")) {
                return Err("Expected insert or delete".into());
            }
            self.backend.fact(
                string(change, "predicate")?,
                change["values"].as_array().ok_or("Expected input values")?,
            )?;
        }
        Ok(changes)
    }

    fn install_pure(&mut self, record: &ProcessorVersion, version: u32) -> Result<Value, String> {
        let prepared = PreparedProgram::new(self.registry.as_ref().unwrap(), record, version)?;
        let result = self.backend.install_source(
            prepared.source,
            prepared.schemas,
            LoweringOptions::for_version(version)?,
        )?;
        self.interface = prepared.interface;
        self.composition = prepared.composition;
        Ok(result)
    }

    pub(crate) fn checkpoint_identity(&self) -> Result<ProgramIdentity, String> {
        let record = self
            .record
            .as_ref()
            .ok_or("Checkpoint requires a pinned instance")?;
        let registry = self
            .registry
            .as_ref()
            .ok_or("Processor registry is not configured")?;
        let verified = registry.get(&record.processor_id, Some(&record.version))?;
        let prepared = PreparedProgram::new(registry, &verified, self.backend.lowering_version())?;
        if self.agent.is_some()
            || prepared.source != self.backend.active_source
            || prepared.schemas != self.backend.schema
        {
            return Err("Installed program does not match its pinned checkpoint identity".into());
        }
        Ok(prepared.identity)
    }

    /// Validate the complete checkpoint against its exact pinned lowering before
    /// Backend compiles anything; activate public admission only after replay.
    pub(crate) fn restore_pinned(
        &mut self,
        bytes: &[u8],
        identity: &ProgramIdentity,
    ) -> Result<(), String> {
        if self.processor.is_some() || self.agent.is_some() {
            return Err("Restore requires a fresh pinned instance".into());
        }
        let registry = self
            .registry
            .as_ref()
            .ok_or("Processor registry is not configured")?;
        let (record, prepared) = prepare_pinned_checkpoint(registry, bytes, identity)?;
        self.backend.restore_checkpoint_bytes(bytes)?;
        self.interface = prepared.interface;
        self.composition = prepared.composition;
        self.processor = Some(json!(identity.processor));
        self.record = Some(record);
        Ok(())
    }

    fn install_processor(&mut self, a: &Value) -> Result<Value, String> {
        if self.backend.health() != "uninitialized" || self.agent.is_some() {
            return Err("Select a processor only in a fresh instance".into());
        }
        self.registry
            .as_ref()
            .ok_or("Processor registry is not configured")?
            .ensure_active(string(a, "processor_id")?)?;
        let record = self
            .registry
            .as_ref()
            .ok_or("Processor registry is not configured")?
            .get(
                string(a, "processor_id")?,
                a.get("version").map(|_| string(a, "version")).transpose()?,
            )?;
        let pinned = record.clone();
        let lowering_version = lowering_version(a, self.backend.lowering_version())?;
        self.backend.set_lowering_version(lowering_version)?;
        let mut result = if let ProcessorDefinition::Program(definition) = &record.definition {
            if let Some(binding) = &definition.operation {
                if !definition.operators.is_empty() {
                    return Err("Typed operators cannot be combined with a registered operation; put the operator in a separate pure program".into());
                }
                let operation = self
                    .operations
                    .get(&binding.name)
                    .ok_or("Pinned operation is not registered on this host")?
                    .clone();
                if operation.version != binding.version
                    || operation.description != binding.description
                {
                    return Err(
                        "Pinned operation definition does not match this host registry".into(),
                    );
                }
                let (agent, result) = AgentProgram::install(
                    &mut self.backend,
                    &binding.name,
                    operation,
                    &definition.rules,
                    definition.schemas.clone(),
                )?;
                self.agent = Some(agent);
                self.interface = definition
                    .interface
                    .as_ref()
                    .map(PublicInterface::from_program);
                result
            } else {
                self.install_pure(&record, lowering_version)?
            }
        } else {
            self.install_pure(&record, lowering_version)?
        };
        self.processor = Some(json!({"processor_id":record.processor_id,"version":record.version}));
        self.record = Some(pinned);
        result["processor"] = self.processor.clone().unwrap();
        if let Some(composition) = &self.composition {
            result["composition"] = serde_json::to_value(composition).map_err(|e| e.to_string())?;
        }
        Ok(result)
    }
}

/// Only these declared ports can be addressed through the ordinary fact tools.
/// Witnesses deliberately expose direct rule bindings through their separate API.
struct PublicInterface {
    inputs: BTreeMap<String, String>,
    outputs: BTreeMap<String, String>,
}
impl PublicInterface {
    fn from_program(interface: &crate::composition::ProgramInterface) -> Self {
        let ports = |names: &[String]| {
            names
                .iter()
                .map(|name| (name.clone(), name.clone()))
                .collect()
        };
        Self {
            inputs: ports(&interface.inputs),
            outputs: ports(&interface.outputs),
        }
    }
    fn changes(&self, changes: &Value) -> Result<Value, String> {
        let mut mapped = changes.as_array().ok_or("Expected changes array")?.clone();
        for change in &mut mapped {
            let predicate = change["predicate"].as_str().ok_or("Missing predicate")?;
            let physical = self
                .inputs
                .get(predicate)
                .ok_or_else(|| format!("Unknown exported input {predicate}"))?;
            change["predicate"] = json!(physical);
        }
        Ok(json!(mapped))
    }
    fn outputs(&self, rows: &str) -> String {
        // DDlog's CLI emits one relation header or row per line; replace only
        // the leading identifier, never a string value containing that name.
        let mut result = String::new();
        for line in rows.lines() {
            for (public, physical) in &self.outputs {
                if let Some(suffix) = line.strip_prefix(&format!("R_{physical}")) {
                    if suffix == ":" || suffix.starts_with('{') {
                        result.push_str(&format!("R_{public}{suffix}\n"));
                        break;
                    }
                }
            }
        }
        result
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("Missing string field: {key}"))
}
/// Optional `lowering_version` argument, validated as a defined version.
fn lowering_version(value: &Value, default: u32) -> Result<u32, String> {
    match value.get("lowering_version") {
        None | Some(Value::Null) => Ok(default),
        Some(version) => {
            let version = version
                .as_u64()
                .and_then(|version| u32::try_from(version).ok())
                .ok_or("lowering_version must be 1 or 2")?;
            LoweringOptions::for_version(version)?;
            Ok(version)
        }
    }
}
fn parse_operators(value: &Value) -> Result<Vec<super::star::Operator>, String> {
    value
        .get("operators")
        .map(|operators| {
            serde_json::from_value(operators.clone())
                .map_err(|error| format!("Invalid operators: {error}"))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operator_arguments_are_typed_and_default_empty() {
        assert!(parse_operators(&json!({})).unwrap().is_empty());
        let value = json!({"operators":[{"type":"large_small_star","vertices":"v","edges":"e","output":"labels"}]});
        let operators = parse_operators(&value).unwrap();
        assert_eq!(operators[0].relations(), ("v", "e", "labels"));
        for value in [
            json!({"operators":null}),
            json!({"operators":[{"type":"native_source","path":"code.rs"}]}),
        ] {
            assert!(parse_operators(&value)
                .unwrap_err()
                .contains("Invalid operators"));
        }
    }
    #[test]
    fn direct_registered_install_rejects_operators_before_build() {
        let backend = Backend::new("unused-star-test".into(), "unused-star-driver".into());
        let mut instance = ProgramInstance::new(backend, BTreeMap::new(), None, None);
        let error = instance.execute("install_agent_program", &json!({
            "operation":"review","rules":"","schemas":{},
            "operators":[{"type":"large_small_star","vertices":"v","edges":"e","output":"labels"}]
        })).unwrap_err();
        assert!(
            error.contains("operators") && error.contains("registered operation"),
            "{error}"
        );
        assert_eq!(instance.backend.health(), "uninitialized");
    }
    #[test]
    fn interface_filters_private_deltas_without_rewriting_values() {
        let interface = PublicInterface {
            inputs: BTreeMap::from([("source".into(), "Input_source".into())]),
            outputs: BTreeMap::from([("result".into(), "Output_result".into())]),
        };
        let text = "Evidence0:\nEvidence0{.v_X = 1}: +1\nR_Module0_private:\nR_Module0_private{.f0 = 1}: +1\nR_Output_result:\nR_Output_result{.f0 = \"R_Output_result\"}: +1\nR_Output_result_other{.f0 = 2}: +1\n";
        assert_eq!(
            interface.outputs(text),
            "R_result:\nR_result{.f0 = \"R_Output_result\"}: +1\n"
        );
        assert!(interface
            .changes(&json!([{"predicate":"Input_source","values":[1],"op":"insert"}]))
            .is_err());
        assert_eq!(
            interface
                .changes(&json!([{"predicate":"source","values":[1],"op":"insert"}]))
                .unwrap(),
            json!([{"predicate":"Input_source","values":[1],"op":"insert"}])
        );
    }
}
