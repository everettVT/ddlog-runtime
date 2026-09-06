//! A registered agent-call primitive. External execution remains in a worker.
//! The language author consumes `agent_result(entity, revision, output)` and
//! does not construct request identities, completion joins, or retry behavior.
use super::{Backend, Result, Schema};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub version: String,
    pub description: String,
}
#[derive(Clone)]
struct Request {
    entity: String,
    revision: i64,
    payload: String,
    claimed: bool,
    output: Option<String>,
}
/// Session-local admission. No expired claim is automatically retried.
pub struct AgentProgram {
    operation: String,
    definition: Operation,
    current: BTreeMap<String, String>,
    requests: BTreeMap<String, Request>,
}
fn schemas() -> BTreeMap<String, Schema> {
    serde_json::from_value(json!({
        "agent_intent":{"input":true,"fields":["string","string","int","string"]},
        "agent_current":{"input":true,"fields":["string","string"]},
        "agent_response":{"input":true,"fields":["string","string"]},
        "agent_claimed":{"input":true,"fields":["string"]},
        "agent_running":{"input":false,"fields":["string"]},
        "agent_finished":{"input":false,"fields":["string"]},
        "agent_pending":{"input":false,"fields":["string","string","int","string"]},
        "agent_result":{"input":false,"fields":["string","int","string"]}
    }))
    .unwrap()
}
/// Pure syntax, type, and supported-lowering validation shared by registry
/// publication and installation. This never builds or activates a runtime.
pub(super) fn lower_registered_program(
    name: &str,
    version: &str,
    rules: &str,
    user_schemas: Value,
) -> Result<(String, BTreeMap<String, Schema>)> {
    super::lower::string_literal(name)?;
    super::lower::string_literal(version)?;
    if name.is_empty() || version.is_empty() {
        return Err("Operation name/version must be nonempty".into());
    }
    let mut declared: BTreeMap<String, Schema> =
        serde_json::from_value(user_schemas).map_err(|e| e.to_string())?;
    if declared.keys().any(|key| key.starts_with("agent_")) {
        return Err("agent_ relations are registered and cannot be redeclared".into());
    }
    // Only the public result relation can be read by authored rules.
    for clause in lemmalog_syntax::ast::parse_program(rules).map_err(|e| e.to_string())? {
        if clause.head.pred.starts_with("agent_") {
            return Err("Registered relations cannot be rule heads".into());
        }
        for lit in clause.body {
            if let lemmalog_syntax::ast::Lit::Pos(a) | lemmalog_syntax::ast::Lit::Neg(a) = lit {
                if a.pred.starts_with("agent_") && a.pred != "agent_result" {
                    return Err(
                        "Read agent_result; internal operation relations are private".into(),
                    );
                }
            }
        }
    }
    declared.extend(schemas());
    let mut source = super::lower(rules, &declared)?;
    source.push_str("\nR_agent_finished(id) :- R_agent_response(id, _).\n\
R_agent_pending(id, entity, revision, payload) :- R_agent_intent(id, entity, revision, payload), R_agent_current(entity, id), not R_agent_finished(id), not R_agent_claimed(id).\n\
R_agent_running(id) :- R_agent_claimed(id), R_agent_intent(id, entity, _, _), R_agent_current(entity, id), not R_agent_finished(id).\n\
R_agent_result(entity, revision, response_value) :- R_agent_intent(id, entity, revision, _), R_agent_current(entity, id), R_agent_response(id, response_value).\n");
    Ok((source, declared))
}

impl AgentProgram {
    pub fn install(
        backend: &mut Backend,
        name: &str,
        definition: Operation,
        rules: &str,
        user_schemas: Value,
    ) -> Result<(Self, Value)> {
        let (source, declared) =
            lower_registered_program(name, &definition.version, rules, user_schemas)?;
        if !backend.facts.is_empty() {
            return Err("Installing a registered operation requires an empty input session".into());
        }
        let result = backend.install_source(source, declared)?;
        Ok((
            Self {
                operation: name.to_string(),
                definition,
                current: BTreeMap::new(),
                requests: BTreeMap::new(),
            },
            result,
        ))
    }
    /// Identity is an injective JSON tuple, not a process-random hash.
    /// Payload and operation version both participate in identity.
    pub fn submit(
        &mut self,
        backend: &mut Backend,
        entity: &str,
        revision: i64,
        payload: &str,
    ) -> Result<Value> {
        super::lower::string_literal(entity)?;
        super::lower::string_literal(payload)?;
        if entity.is_empty() || revision < 0 {
            return Err("Expected nonempty entity and nonnegative revision".into());
        }
        if let Some(previous) = self
            .current
            .get(entity)
            .and_then(|id| self.requests.get(id))
        {
            if revision < previous.revision {
                return Err("Revision moved backwards".into());
            }
            if revision == previous.revision && payload != previous.payload {
                return Err("Same revision has different input; advance revision".into());
            }
        }
        let id = json!([
            self.operation,
            self.definition.version,
            entity,
            revision,
            payload
        ])
        .to_string();
        let mut changes = Vec::new();
        if let Some(old) = self.current.get(entity) {
            if old != &id {
                changes
                    .push(json!({"op":"delete","predicate":"agent_current","values":[entity,old]}));
            }
        }
        changes.push(
            json!({"op":"insert","predicate":"agent_intent","values":[id,entity,revision,payload]}),
        );
        changes.push(json!({"op":"insert","predicate":"agent_current","values":[entity,id]}));
        let result = backend.apply(&json!(changes))?;
        self.current.insert(entity.to_string(), id.clone());
        self.requests.entry(id.clone()).or_insert(Request {
            entity: entity.to_string(),
            revision,
            payload: payload.to_string(),
            claimed: false,
            output: None,
        });
        Ok(
            json!({"request_id":id,"operation":self.operation,"operation_version":self.definition.version,"transaction":result}),
        )
    }
    pub fn claim(&mut self, backend: &mut Backend, id: &str) -> Result<Value> {
        if backend.runtime.is_none() {
            return Err("Runtime unavailable; request freshness cannot be established".into());
        }
        let request = self.requests.get_mut(id).ok_or("Unknown request")?;
        if self.current.get(&request.entity).map(String::as_str) != Some(id) {
            return Err("Stale request cannot be admitted".into());
        }
        if request.claimed || request.output.is_some() {
            return Err("Already claimed or completed; no automatic replay".into());
        }
        backend.apply(&json!([{"op":"insert","predicate":"agent_claimed","values":[id]}]))?;
        request.claimed = true;
        Ok(
            json!({"request_id":id,"operation":self.operation,"operation_version":self.definition.version,"entity":request.entity,"revision":request.revision,"payload":request.payload,"status":"claimed","recovery":"Session-local claim; uncertain outcomes require reconciliation, not automatic retries"}),
        )
    }
    pub fn complete(&mut self, backend: &mut Backend, id: &str, output: &str) -> Result<Value> {
        super::lower::string_literal(output)?;
        let request = self.requests.get_mut(id).ok_or("Unknown request")?;
        if !request.claimed {
            return Err("Request must be claimed before completion".into());
        }
        if let Some(old) = &request.output {
            if old != output {
                return Err("Conflicting completion for request".into());
            }
            return Ok(
                json!({"request_id":id,"duplicate":true,"fresh":self.current.get(&request.entity).map(String::as_str)==Some(id)}),
            );
        }
        let result = backend
            .apply(&json!([{"op":"insert","predicate":"agent_response","values":[id,output]}]))?;
        request.output = Some(output.to_string());
        Ok(
            json!({"request_id":id,"duplicate":false,"fresh":self.current.get(&request.entity).map(String::as_str)==Some(id),"transaction":result}),
        )
    }
    pub fn status(&self) -> Value {
        json!({"operation":self.operation,"operation_version":self.definition.version,"requests":self.requests.iter().map(|(id,r)|json!({"request_id":id,"entity":r.entity,"revision":r.revision,"fresh":self.current.get(&r.entity)==Some(id),"status":if r.output.is_some(){"completed"}else if r.claimed{"claimed"}else{"pending"}})).collect::<Vec<_>>(),"durability":"session-local"})
    }
}
