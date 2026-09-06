//! Pure, typed composition of exact leaf processor versions in one DDlog graph.
//! Connections become rules in the same parsed AST; no runtime copies facts
//! between processors and no current-version pointer participates in lowering.
use super::lower::{ident, lower_clauses_with_operators};
use super::registry::{
    ProcessorDefinition, ProcessorReference, ProcessorVersion, ProgramDefinition,
};
use super::star::Operator;
use super::Schema;
use lemmalog_syntax::ast::{parse_program, Atom, Clause, Lit};
use lemmalog_syntax::Term;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProgramInterface {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub node: String,
    pub relation: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalInput {
    pub fields: Vec<String>,
    /// Explicit broadcast. Every target still has exactly one source.
    pub targets: Vec<Endpoint>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub from: Endpoint,
    pub to: Endpoint,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompositionManifest {
    pub nodes: BTreeMap<String, ProcessorReference>,
    pub inputs: BTreeMap<String, ExternalInput>,
    pub bindings: Vec<Binding>,
    pub outputs: BTreeMap<String, Endpoint>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CompositionResolution {
    pub nodes: BTreeMap<String, ProcessorReference>,
    pub dependencies: BTreeMap<String, ProcessorReference>,
    pub generated_source_sha256: String,
    /// Public interface names mapped to deterministic generated relation names.
    pub inputs: BTreeMap<String, String>,
    pub outputs: BTreeMap<String, String>,
    /// Index equals the zero-based generated Evidence relation index.
    pub rules: Vec<Value>,
    pub relations: BTreeMap<String, Value>,
}
#[derive(Clone, Debug)]
pub struct CompiledComposition {
    pub source: String,
    pub schemas: BTreeMap<String, Schema>,
    pub resolution: CompositionResolution,
}

/// Interface types are the existing declared schemas, never a second type system.
/// Every input must be declared; unexported derived relations remain private.
pub fn validate_interface(program: &ProgramDefinition) -> Result<()> {
    let Some(interface) = &program.interface else {
        return Ok(());
    };
    let schemas: BTreeMap<String, Schema> =
        serde_json::from_value(program.schemas.clone()).map_err(|error| error.to_string())?;
    let mut inputs = BTreeSet::new();
    for name in &interface.inputs {
        if !inputs.insert(name.clone()) {
            return Err(format!("Duplicate interface input {name}"));
        }
        if !schemas.get(name).is_some_and(|schema| schema.input) {
            return Err(format!(
                "Interface input {name} must name a declared input relation"
            ));
        }
    }
    for (name, schema) in &schemas {
        if schema.input && !inputs.contains(name) {
            return Err(format!(
                "Input relation {name} is missing from the interface"
            ));
        }
    }
    let mut outputs = BTreeSet::new();
    for name in &interface.outputs {
        if !outputs.insert(name.clone()) {
            return Err(format!("Duplicate interface output {name}"));
        }
        if !schemas.get(name).is_some_and(|schema| !schema.input) {
            return Err(format!(
                "Interface output {name} must name a declared derived relation"
            ));
        }
    }
    if outputs.is_empty() {
        return Err("An explicit processor interface must export at least one output".into());
    }
    Ok(())
}

/// Exact, integrity-checked definition tree supplied by the registry resolver.
#[derive(Clone, Debug)]
pub struct ResolvedNode {
    pub record: ProcessorVersion,
    pub children: BTreeMap<String, ResolvedNode>,
}
#[derive(Clone)]
struct Port {
    name: String,
    fields: Vec<String>,
}
struct Ports {
    inputs: BTreeMap<String, Port>,
    outputs: BTreeMap<String, Port>,
}
fn qualified(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_string()
    } else {
        format!("{scope}.{name}")
    }
}
fn absolute(scope: &str, endpoint: &Endpoint) -> Endpoint {
    Endpoint {
        node: qualified(scope, &endpoint.node),
        relation: endpoint.relation.clone(),
    }
}
fn endpoint<'a>(
    nodes: &'a BTreeMap<String, Ports>,
    value: &Endpoint,
    input: bool,
    scope: &str,
) -> Result<&'a Port> {
    let location = qualified(scope, &value.node);
    let node = nodes
        .get(&value.node)
        .ok_or_else(|| format!("Unknown node {location} at {location}.{}", value.relation))?;
    let exposed = if input { &node.inputs } else { &node.outputs };
    exposed.get(&value.relation).ok_or_else(|| {
        format!(
            "{location}.{} is not an exported {}",
            value.relation,
            if input { "input" } else { "output" }
        )
    })
}
fn bridge(to: &str, from: &str, arity: usize) -> Clause {
    let args: Vec<_> = (0..arity).map(|i| Term::Var(format!("X{i}"))).collect();
    Clause {
        name: None,
        head: Atom {
            pred: to.to_string(),
            args: args.clone(),
        },
        body: vec![Lit::Pos(Atom {
            pred: from.to_string(),
            args,
        })],
        is_fact: false,
    }
}
fn compatible(from: &str, source: &[String], to: &Endpoint, destination: &[String]) -> Result<()> {
    if source != destination {
        return Err(format!(
            "Binding type mismatch: {from} has {source:?}, {}.{} requires {destination:?}",
            to.node, to.relation
        ));
    }
    Ok(())
}
struct Expansion {
    schemas: BTreeMap<String, Schema>,
    clauses: Vec<Clause>,
    operators: Vec<Operator>,
    origins: Vec<Value>,
    relations: BTreeMap<String, Value>,
    dependencies: BTreeMap<String, ProcessorReference>,
    next_node: usize,
}
impl Expansion {
    fn program(
        &mut self,
        program: &ProgramDefinition,
        reference: &ProcessorReference,
        path: &str,
        index: usize,
    ) -> Result<Ports> {
        if program.operation.is_some() {
            return Err(format!("Node {path}: external-operation composition is not supported by this runtime's single-operation contract; install the program independently using request/response tools"));
        }
        validate_interface(program).map_err(|error| format!("Node {path}: {error}"))?;
        let interface = program
            .interface
            .as_ref()
            .ok_or_else(|| format!("Node {path} requires an explicit interface"))?;
        let schemas: BTreeMap<String, Schema> =
            serde_json::from_value(program.schemas.clone()).map_err(|error| error.to_string())?;
        super::lower_with_operators(&program.rules, &schemas, &program.operators)
            .map_err(|error| format!("Node {path}: {error}"))?;
        let names: BTreeMap<_, _> = schemas
            .keys()
            .map(|name| (name.clone(), format!("Module{index}_{name}")))
            .collect();
        for (name, schema) in &schemas {
            self.schemas.insert(
                names[name].clone(),
                Schema {
                    input: false,
                    fields: schema.fields.clone(),
                },
            );
            self.relations.insert(names[name].clone(), json!({"kind":"processor_relation","node":path,"processor_id":reference.processor_id,"version":reference.version,"relation":name,"schema":schema}));
        }
        self.operators.extend(
            program
                .operators
                .iter()
                .map(|operator| operator.renamed(&names)),
        );
        for (rule, mut clause) in parse_program(&program.rules)
            .map_err(|error| error.to_string())?
            .into_iter()
            .enumerate()
        {
            self.origins.push(json!({"kind":"processor_rule","node":path,"processor_id":reference.processor_id,"version":reference.version,"rule":rule,"name":clause.name,"head":clause.head.pred}));
            clause.head.pred = names[&clause.head.pred].clone();
            for literal in &mut clause.body {
                if let Lit::Pos(atom) = literal {
                    atom.pred = names[&atom.pred].clone();
                }
            }
            self.clauses.push(clause);
        }
        let port = |name: &String| {
            (
                name.clone(),
                Port {
                    name: names[name].clone(),
                    fields: schemas[name].fields.clone(),
                },
            )
        };
        Ok(Ports {
            inputs: interface.inputs.iter().map(port).collect(),
            outputs: interface.outputs.iter().map(port).collect(),
        })
    }

    fn manifest(
        &mut self,
        manifest: &CompositionManifest,
        programs: &BTreeMap<String, ResolvedNode>,
        scope: &str,
        index: Option<usize>,
        owner: Option<&ProcessorReference>,
    ) -> Result<Ports> {
        if manifest.nodes.is_empty() || manifest.outputs.is_empty() {
            return Err("Composition requires at least one node and exported output".into());
        }
        if manifest.nodes.keys().ne(programs.keys()) {
            return Err("Resolved nodes must exactly match the composition manifest".into());
        }
        let mut nodes = BTreeMap::new();
        for (alias, reference) in &manifest.nodes {
            if !ident(alias) {
                return Err(format!("Invalid node alias {alias}"));
            }
            let path = qualified(scope, alias);
            let resolved = &programs[alias];
            if reference.processor_id != resolved.record.processor_id
                || reference.version != resolved.record.version
            {
                return Err(format!("Resolved version mismatch for node {path}"));
            }
            let child_index = self.next_node;
            self.next_node += 1;
            self.dependencies.insert(path.clone(), reference.clone());
            let ports = match &resolved.record.definition {
                ProcessorDefinition::Program(program) => {
                    self.program(program, reference, &path, child_index)?
                }
                ProcessorDefinition::Composition(definition) => self.manifest(
                    &definition.composition,
                    &resolved.children,
                    &path,
                    Some(child_index),
                    Some(reference),
                )?,
            };
            nodes.insert(alias.clone(), ports);
        }
        let mut assigned = BTreeSet::new();
        let mut inputs = BTreeMap::new();
        for (name, input) in &manifest.inputs {
            if !ident(name)
                || input.fields.is_empty()
                || input.targets.is_empty()
                || input
                    .fields
                    .iter()
                    .any(|kind| kind != "int" && kind != "string")
            {
                return Err(format!("External input {} requires a valid name, positive int/string arity and at least one target", qualified(scope, name)));
            }
            if manifest.outputs.contains_key(name) {
                return Err(format!(
                    "Ambiguous external name {}: both input and output",
                    qualified(scope, name)
                ));
            }
            let physical = match index {
                None => format!("Input_{name}"),
                Some(index) => format!("Composite{index}_Input_{name}"),
            };
            self.schemas.insert(
                physical.clone(),
                Schema {
                    input: index.is_none(),
                    fields: input.fields.clone(),
                },
            );
            self.relations.insert(physical.clone(), json!({"kind":"external_input","scope":scope,"owner":owner,"input":name,"fields":input.fields}));
            for target in &input.targets {
                let to = endpoint(&nodes, target, true, scope)?;
                compatible(
                    &format!("input {}", qualified(scope, name)),
                    &input.fields,
                    &absolute(scope, target),
                    &to.fields,
                )?;
                if !assigned.insert(target.clone()) {
                    return Err(format!("Multiple sources for input {}.{}; declare a separate union program to combine sources", qualified(scope, &target.node), target.relation));
                }
                self.clauses
                    .push(bridge(&to.name, &physical, to.fields.len()));
                self.origins.push(json!({"kind":"input_binding","scope":scope,"owner":owner,"input":name,"to":absolute(scope, target)}));
            }
            inputs.insert(
                name.clone(),
                Port {
                    name: physical,
                    fields: input.fields.clone(),
                },
            );
        }
        for binding in &manifest.bindings {
            let from = endpoint(&nodes, &binding.from, false, scope)?;
            let to = endpoint(&nodes, &binding.to, true, scope)?;
            compatible(
                &format!(
                    "{}.{}",
                    qualified(scope, &binding.from.node),
                    binding.from.relation
                ),
                &from.fields,
                &absolute(scope, &binding.to),
                &to.fields,
            )?;
            if !assigned.insert(binding.to.clone()) {
                return Err(format!("Multiple sources for input {}.{}; declare a separate union program to combine sources", qualified(scope, &binding.to.node), binding.to.relation));
            }
            self.clauses
                .push(bridge(&to.name, &from.name, from.fields.len()));
            self.origins.push(json!({"kind":"processor_binding","scope":scope,"owner":owner,"from":absolute(scope, &binding.from),"to":absolute(scope, &binding.to)}));
        }
        for (alias, node) in &nodes {
            for relation in node.inputs.keys() {
                if !assigned.contains(&Endpoint {
                    node: alias.clone(),
                    relation: relation.clone(),
                }) {
                    return Err(format!("Unconnected input {}.{relation}: bind an external input or exported processor output", qualified(scope, alias)));
                }
            }
        }
        let mut outputs = BTreeMap::new();
        for (name, output) in &manifest.outputs {
            if !ident(name) {
                return Err(format!(
                    "Invalid external output name {}",
                    qualified(scope, name)
                ));
            }
            let from = endpoint(&nodes, output, false, scope)?;
            let physical = match index {
                None => format!("Output_{name}"),
                Some(index) => format!("Composite{index}_Output_{name}"),
            };
            self.schemas.insert(
                physical.clone(),
                Schema {
                    input: false,
                    fields: from.fields.clone(),
                },
            );
            self.clauses
                .push(bridge(&physical, &from.name, from.fields.len()));
            self.origins.push(json!({"kind":"output_binding","scope":scope,"owner":owner,"output":name,"from":absolute(scope, output)}));
            self.relations.insert(physical.clone(), json!({"kind":"external_output","scope":scope,"owner":owner,"output":name,"from":absolute(scope, output),"fields":from.fields}));
            outputs.insert(
                name.clone(),
                Port {
                    name: physical,
                    fields: from.fields.clone(),
                },
            );
        }
        Ok(Ports { inputs, outputs })
    }
}

/// Composition is source expansion. The ordinary lowerer decides which final
/// relational programs are supported; there is no additional node-cycle rule.
pub fn compile_resolved(
    manifest: &CompositionManifest,
    programs: &BTreeMap<String, ResolvedNode>,
) -> Result<CompiledComposition> {
    let mut expansion = Expansion {
        schemas: BTreeMap::new(),
        clauses: Vec::new(),
        operators: Vec::new(),
        origins: Vec::new(),
        relations: BTreeMap::new(),
        dependencies: BTreeMap::new(),
        next_node: 0,
    };
    let ports = expansion.manifest(manifest, programs, "", None, None)?;
    let source =
        lower_clauses_with_operators(&expansion.clauses, &expansion.schemas, &expansion.operators)?;
    let resolution = CompositionResolution {
        nodes: manifest.nodes.clone(),
        dependencies: expansion.dependencies,
        generated_source_sha256: format!("{:x}", Sha256::digest(source.as_bytes())),
        inputs: ports
            .inputs
            .into_iter()
            .map(|(public, port)| (public, port.name))
            .collect(),
        outputs: ports
            .outputs
            .into_iter()
            .map(|(public, port)| (public, port.name))
            .collect(),
        rules: expansion.origins,
        relations: expansion.relations,
    };
    Ok(CompiledComposition {
        source,
        schemas: expansion.schemas,
        resolution,
    })
}

/// Convenience entry point for already-resolved leaf records.
pub fn compile(
    manifest: &CompositionManifest,
    programs: &BTreeMap<String, ProcessorVersion>,
) -> Result<CompiledComposition> {
    let nodes = programs
        .iter()
        .map(|(alias, record)| {
            (
                alias.clone(),
                ResolvedNode {
                    record: record.clone(),
                    children: BTreeMap::new(),
                },
            )
        })
        .collect();
    compile_resolved(manifest, &nodes)
}
