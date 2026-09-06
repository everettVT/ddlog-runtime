//! Program state and semantic operations, independent of MCP and connections.
use crate::composition::CompositionResolution;
use crate::registry::{GitProvenance, ProcessorDefinition, ProcessorRegistry};
use crate::{AgentProgram, Backend, Operation};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// One semantic owner whose caller controls its lifetime independently of connections.
pub struct ProgramInstance {
    pub(super) backend: Backend,
    pub(crate) operations: BTreeMap<String, Operation>,
    agent: Option<AgentProgram>,
    pub(crate) registry: Option<ProcessorRegistry>,
    pub(crate) instance_id: Option<String>,
    processor: Option<Value>,
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
            interface: None,
            composition: None,
        }
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
            return Ok(
                json!({"instance_id":self.instance_id,"health":self.backend.health(),"processor":self.processor,"composition":self.composition}),
            );
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
                if let Some(interface) = &self.interface {
                    let changes = interface.changes(&a["changes"])?;
                    let mut result = self.backend.apply(&changes)?;
                    result["deltas"] =
                        json!(interface.outputs(result["deltas"].as_str().unwrap_or("")));
                    Ok(result)
                } else {
                    self.backend.apply(&a["changes"])
                }
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
            "processor_create" => registry.create(definition()?, provenance()?)?,
            "processor_publish" => registry.publish(
                string(a, "processor_id")?,
                definition()?,
                string(a, "expected_version")?,
                provenance()?,
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
        let mut result = match record.definition {
            ProcessorDefinition::Composition(definition) => {
                let compiled = self
                    .registry
                    .as_ref()
                    .unwrap()
                    .compile_composition(&definition.composition)?;
                let result = self
                    .backend
                    .install_source(compiled.source, compiled.schemas)?;
                self.interface = Some(PublicInterface {
                    inputs: compiled.resolution.inputs.clone(),
                    outputs: compiled.resolution.outputs.clone(),
                });
                self.composition = Some(compiled.resolution);
                result
            }
            ProcessorDefinition::Program(definition) => {
                let result = if let Some(binding) = definition.operation {
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
                        definition.schemas,
                    )?;
                    self.agent = Some(agent);
                    result
                } else {
                    self.backend.install_with_operators(
                        &definition.rules,
                        definition.schemas,
                        &definition.operators,
                    )?
                };
                self.interface = definition.interface.map(|interface| PublicInterface {
                    inputs: interface
                        .inputs
                        .into_iter()
                        .map(|name| (name.clone(), name))
                        .collect(),
                    outputs: interface
                        .outputs
                        .into_iter()
                        .map(|name| (name.clone(), name))
                        .collect(),
                });
                result
            }
        };
        self.processor = Some(json!({"processor_id":record.processor_id,"version":record.version}));
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
