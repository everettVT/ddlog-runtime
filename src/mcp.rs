//! MCP schemas and JSON-RPC adaptation for the transport-independent owner.
use crate::ProgramInstance;
use serde_json::{json, Value};

fn operator_schema() -> Value {
    json!({"type":"array","items":{"type":"object","properties":{
        "type":{"const":"large_small_star"},"vertices":{"type":"string"},
        "edges":{"type":"string"},"output":{"type":"string"}
    },"required":["type","vertices","edges","output"],"additionalProperties":false}})
}

fn tools() -> Value {
    json!([
        {"name":"lemmalog_install_rules","description":"Compile and atomically replace this session's typed positive program, including optional built-in Large-Star/Small-Star operators; replay retained facts. Unsupported syntax is rejected.","inputSchema":{"type":"object","properties":{"rules":{"type":"string"},"schemas":{"type":"object","additionalProperties":{"type":"object","properties":{"input":{"type":"boolean"},"fields":{"type":"array","items":{"enum":["int","string"]},"minItems":1}},"required":["input","fields"],"additionalProperties":false}},"operators":operator_schema()},"required":["rules","schemas"],"additionalProperties":false}},
        {"name":"apply_changes","description":"Transactionally insert or delete input facts with set semantics.","inputSchema":{"type":"object","properties":{"changes":{"type":"array","items":{"type":"object","properties":{"op":{"enum":["insert","delete"]},"predicate":{"type":"string"},"values":{"type":"array","items":{"type":["integer","string"]}}},"required":["op","predicate","values"],"additionalProperties":false}}},"required":["changes"],"additionalProperties":false}},
        {"name":"lemmalog_query","description":"Dump a declared output relation at the last completed transaction. Returns DDlog row text.","inputSchema":{"type":"object","properties":{"predicate":{"type":"string"}},"required":["predicate"],"additionalProperties":false}},
        {"name":"lemmalog_why","description":"Read direct variable-binding witnesses for a zero-based ordinary rule index. Typed operators have no Evidence row. Not recursive provenance.","inputSchema":{"type":"object","properties":{"rule":{"type":"integer","minimum":0}},"required":["rule"],"additionalProperties":false}}
    ])
}
fn agent_tools() -> Value {
    fn tool(name: &str, description: &str, properties: Value, required: Value) -> Value {
        json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
    }
    json!([
        tool("agent_operations","Discover operator-registered operations. Each takes and returns a string; providers execute outside DDlog.",json!({}),json!([])),
        tool("install_agent_program","Select a registered operation. Author rules consuming agent_result(entity:string, revision:int, output:string). The runtime generates private request/response relations and freshness joins.",json!({"operation":{"type":"string"},"rules":{"type":"string"},"schemas":{"type":"object"}}),json!(["operation","rules","schemas"])),
        tool("submit_agent_input","Submit a versioned input; identity includes operation version, entity, revision and exact payload. Same revision cannot change payload.",json!({"entity":{"type":"string"},"revision":{"type":"integer","minimum":0},"payload":{"type":"string"}}),json!(["entity","revision","payload"])),
        tool("claim_agent_request","Admit external work once per session. Stale or already claimed requests are rejected. No automatic replay after uncertain outcomes.",json!({"request_id":{"type":"string"}}),json!(["request_id"])),
        tool("complete_agent_request","Record the external worker's response. Stale replies are retained but never join current outputs; conflicting responses are rejected.",json!({"request_id":{"type":"string"},"output":{"type":"string"}}),json!(["request_id","output"])),
        tool("agent_request_status","Inspect per-request identity, status and freshness. State is session-local, not durable.",json!({}),json!([]))
    ])
}

pub fn handle_line(instance: &mut ProgramInstance, line: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(line) {
        Ok(message) => handle(instance, message),
        Err(_) => Some(rpc_error(Value::Null, -32700, "Invalid JSON")),
    }
}

fn handle(instance: &mut ProgramInstance, message: Value) -> Option<Value> {
    if !message.is_object() || message["jsonrpc"] != "2.0" || !message["method"].is_string() {
        return Some(rpc_error(Value::Null, -32600, "Invalid JSON-RPC request"));
    }
    let id = message.get("id")?.clone(); // Notifications do not have replies.
    if !id.is_string() && !id.is_i64() && !id.is_u64() {
        return Some(rpc_error(Value::Null, -32600, "Invalid request ID"));
    }
    let result = match message["method"].as_str().unwrap() {
        "initialize" => {
            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"lemmalog-ddlog","version":"0.2.0"}})
        }
        "ping" => json!({}),
        "tools/list" => {
            let mut list = tools();
            if !instance.operations.is_empty() {
                list.as_array_mut()
                    .unwrap()
                    .extend(agent_tools().as_array().unwrap().iter().cloned());
            }
            if instance.instance_id.is_some() {
                list.as_array_mut().unwrap().push(json!({"name":"instance_info","description":"Inspect this shared in-memory instance and its pinned processor; no recovery or replay.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}));
            }
            if instance.registry.is_some() {
                list.as_array_mut()
                    .unwrap()
                    .extend(registry_tools().as_array().unwrap().iter().cloned());
            }
            json!({"tools":list})
        }
        "tools/call" => {
            let name = message["params"]["name"].as_str().unwrap_or("");
            let args = &message["params"]["arguments"];
            match instance.execute(name, args) {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"isError":false})
                }
                Err(error) => {
                    json!({"content":[{"type":"text","text":actionable_error(name, &error)}],"isError":true})
                }
            }
        }
        _ => return Some(rpc_error(id, -32601, "Unknown method")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

/// Cause/state stays intact; the tool surface supplies a bounded next action.
/// This never turns an uncertain result into authorization to retry a mutation.
fn actionable_error(tool: &str, error: &str) -> String {
    if error.contains("Next action:") {
        return error.to_string();
    }
    let action = if error.contains("Runtime unavailable") || error.contains("uncertain") {
        "Inspect instance_info and reconcile the uncertain operation before deciding whether to create a new instance; do not blindly retry it."
    } else if tool == "processor_archive" || tool == "processor_restore" {
        "Read processor_search with the processor identity and include_archived=true; reconsider the change using its current version and lifecycle_revision before submitting new preconditions."
    } else if error.contains("compilation failed") {
        "Inspect the reported build log for compiler diagnostics; correct the saved definition or operator toolchain, then explicitly install a valid version in a fresh instance."
    } else if error.contains("conflict") || error.contains("conflict:") {
        "Read the latest processor version and reconsider the intended edit before submitting a new conditional publication."
    } else {
        match tool {
            "processor_create" | "processor_publish" => "Correct the reported syntax, schema or connection in definition; use an exported endpoint with matching field types and exactly one source per input. Inspect tools/list for the accepted definition shapes, then submit the corrected definition.",
            "processor_list" | "processor_search" => "Use a limit from 1 through 100 and pass the preceding next_cursor as after with the same query and include_archived option.",
            "processor_get" | "processor_fork" => "Discover the identity with processor_list or processor_search (include_archived=true for history), then read a valid exact version using processor_get.",
            "processor_install" => "Inspect instance_info and processor_get for the intended exact version; select an active definition and install it in a fresh instance.",
            "lemmalog_query" => "Read the pinned definition or composition metadata using processor_get, then query a declared exported output name.",
            "lemmalog_why" => "Read composition.rules from instance_info or processor_get and choose an existing zero-based rule index; ordinary programs use their authored rule order.",
            "apply_changes" => "Correct the input predicate, operation and field values using the declared input schemas before submitting the transaction.",
            _ => "Inspect tools/list for supported tools and required argument shapes, then correct the reported request.",
        }
    };
    format!("{error}\nNext action: {action}")
}

pub(super) fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn registry_tools() -> Value {
    let endpoint = json!({"type":"object","properties":{"node":{"type":"string"},"relation":{"type":"string"}},"required":["node","relation"],"additionalProperties":false});
    let reference = json!({"type":"object","properties":{"processor_id":{"type":"string"},"version":{"type":"string"}},"required":["processor_id","version"],"additionalProperties":false});
    let interface = json!({"type":"object","properties":{"inputs":{"type":"array","items":{"type":"string"}},"outputs":{"type":"array","items":{"type":"string"},"minItems":1}},"required":["inputs","outputs"],"additionalProperties":false});
    let definition = json!({"oneOf":[
        {"type":"object","properties":{"rules":{"type":"string"},"schemas":{"type":"object"},"interface":interface,"operators":operator_schema(),"operation":{"type":["object","null"],"properties":{"name":{"type":"string"},"version":{"type":"string"},"description":{"type":"string"}},"required":["name","version","description"],"additionalProperties":false}},"required":["rules","schemas"],"additionalProperties":false},
        {"type":"object","properties":{"composition":{"type":"object","properties":{
            "nodes":{"type":"object","minProperties":1,"additionalProperties":reference},
            "inputs":{"type":"object","additionalProperties":{"type":"object","properties":{"fields":{"type":"array","items":{"enum":["int","string"]},"minItems":1},"targets":{"type":"array","items":endpoint,"minItems":1}},"required":["fields","targets"],"additionalProperties":false}},
            "bindings":{"type":"array","items":{"type":"object","properties":{"from":endpoint,"to":endpoint},"required":["from","to"],"additionalProperties":false}},
            "outputs":{"type":"object","minProperties":1,"additionalProperties":endpoint}
        },"required":["nodes","inputs","bindings","outputs"],"additionalProperties":false}},"required":["composition"],"additionalProperties":false}
    ]});
    let tool = |name: &str, description: &str, properties: Value, required: Value| json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}});
    let discovery = json!({"limit":{"type":"integer","minimum":1,"maximum":100,"default":20},"after":{"type":"string"},"include_archived":{"type":"boolean","default":false}});
    let mut search = discovery.clone();
    search["query"] = json!({"type":"string"});
    json!([
        tool("processor_list","List saved processors ordered by stable identity, with bounded keyset pagination. Pass next_cursor as after; concurrent changes do not form a snapshot. Archived entries are omitted unless include_archived is true.",discovery,json!([])),
        tool("processor_search","Case-insensitive literal substring search across identity, version and compact authored definition JSON. Same pagination and archive rules as processor_list; empty query lists all.",search,json!(["query"])),
        tool("processor_archive","Conditionally archive using both the expected code version and lifecycle revision. Retains code/lineage and existing instances/composition references. Same-state at current revision is a no-op; stale revisions conflict.",json!({"processor_id":{"type":"string"},"expected_version":{"type":"string"},"expected_revision":{"type":"integer","minimum":0}}),json!(["processor_id","expected_version","expected_revision"])),
        tool("processor_restore","Conditionally restore an archived identity to active discovery and new use, using its expected code version and lifecycle revision. Preserves versions/pins and does not compile or run. Same-state at current revision is a no-op; stale revisions conflict.",json!({"processor_id":{"type":"string"},"expected_version":{"type":"string"},"expected_revision":{"type":"integer","minimum":0}}),json!(["processor_id","expected_version","expected_revision"])),
        tool("processor_create","Validate and save a program under a new stable identity. Supply rules or a composition manifest referencing exact immutable pure program versions, including other composed programs. Returns resolved dependencies and witness origins. Does not compile or activate a graph.",json!({"definition":definition,"git_provenance":{"type":"object"}}),json!(["definition"])),
        tool("processor_publish","Validate syntax and types, then save a definition and move current only if expected_version matches. Does not activate a graph.",json!({"processor_id":{"type":"string"},"expected_version":{"type":"string"},"definition":definition,"git_provenance":{"type":"object"}}),json!(["processor_id","expected_version","definition"])),
        tool("processor_fork","Create a new processor identity referencing an exact source version as lineage.",json!({"processor_id":{"type":"string"},"version":{"type":"string"},"git_provenance":{"type":"object"}}),json!(["processor_id","version"])),
        tool("processor_get","Read an immutable version; omitted version resolves current once.",json!({"processor_id":{"type":"string"},"version":{"type":"string"}}),json!(["processor_id"])),
        tool("processor_install","Compile, activate and pin a program version in a fresh instance. A composition runs as one graph; current pointer updates cannot change it.",json!({"processor_id":{"type":"string"},"version":{"type":"string"}}),json!(["processor_id"]))
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operator_schema_is_shared_by_direct_and_saved_programs() {
        let direct = tools();
        let saved = registry_tools();
        let create = saved
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "processor_create")
            .unwrap();
        assert_eq!(
            direct[0]["inputSchema"]["properties"]["operators"],
            create["inputSchema"]["properties"]["definition"]["oneOf"][0]["properties"]
                ["operators"]
        );
        assert_eq!(
            operator_schema()["items"]["properties"]["type"]["const"],
            "large_small_star"
        );
        assert_eq!(operator_schema()["items"]["additionalProperties"], false);
    }
}
