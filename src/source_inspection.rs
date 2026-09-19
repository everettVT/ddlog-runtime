//! Pure authored-logic inspection. No Backend, native process, or registry writes.
//! This is a derived, versioned view; it is never included in definition hashes.
use crate::registry::{ProcessorDefinition, ProcessorVersion, ProgramDefinition};
use crate::syntax::ast::{parse_program_spanned, Atom, CmpOp, Expr, Lit};
use crate::syntax::Term;
use crate::Schema;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Argument {
    Variable {
        name: String,
        field_type: Option<String>,
    },
    String {
        value: String,
    },
    Integer {
        value: i64,
    },
    Wildcard,
    Unsupported {
        source: String,
    },
}

#[derive(Debug, Serialize)]
pub struct RelationMatch {
    pub relation: String,
    pub arguments: Vec<Argument>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Condition {
    Positive {
        atom: RelationMatch,
    },
    Negative {
        atom: RelationMatch,
    },
    Comparison {
        operator: String,
        left: Argument,
        right: Argument,
    },
    Unsupported {
        description: String,
    },
}

#[derive(Debug, Serialize)]
pub struct Rule {
    pub id: String,
    pub name: Option<String>,
    pub is_fact: bool,
    pub head: RelationMatch,
    pub conditions: Vec<Condition>,
    pub variables: BTreeMap<String, String>,
    pub source_start: usize,
    pub source_end: usize,
    pub source: String,
}

fn argument(term: &Term, types: &BTreeMap<String, String>) -> Argument {
    match term {
        Term::Var(name) => Argument::Variable {
            name: name.clone(),
            field_type: types.get(name).cloned(),
        },
        Term::Sym(value) => Argument::String {
            value: value.clone(),
        },
        Term::Int(value) => Argument::Integer { value: *value },
        Term::Wildcard => Argument::Wildcard,
        Term::Agg(..) => Argument::Unsupported {
            source: term.render(),
        },
    }
}

fn atom(atom: &Atom, types: &BTreeMap<String, String>) -> RelationMatch {
    RelationMatch {
        relation: atom.pred.clone(),
        arguments: atom.args.iter().map(|t| argument(t, types)).collect(),
    }
}

/// Inspect the shipped source language, using the same support/type checks as execution.
/// Parsed unsupported forms (including inline facts) remain visible with diagnostics.
pub fn inspect_program(program: &ProgramDefinition) -> Value {
    let mut diagnostics = Vec::<String>::new();
    let schemas: BTreeMap<String, Schema> = match serde_json::from_value(program.schemas.clone()) {
        Ok(value) => value,
        Err(error) => {
            diagnostics.push(format!("Invalid schemas: {error}"));
            BTreeMap::new()
        }
    };
    if let Err(error) = crate::composition::validate_interface(program) {
        diagnostics.push(error);
    }
    if let Err(error) = crate::lower_with_operators(&program.rules, &schemas, &program.operators) {
        diagnostics.push(error);
    }
    let clauses = match parse_program_spanned(&program.rules) {
        Ok(value) => value,
        Err(error) => {
            diagnostics.push(error.to_string());
            Vec::new()
        }
    };
    let mut rules = Vec::new();
    let mut dependencies = Vec::new();
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (index, spanned) in clauses.iter().enumerate() {
        let clause = &spanned.clause;
        let mut types = BTreeMap::new();
        // Project positional types from declared positive matches. Admission above is
        // authoritative; this map is not another type checker.
        for literal in &clause.body {
            if let Lit::Pos(a) = literal {
                if let Some(schema) = schemas.get(&a.pred) {
                    for (term, field) in a.args.iter().zip(&schema.fields) {
                        if let Term::Var(name) = term {
                            types.insert(name.clone(), field.clone());
                        }
                    }
                }
            }
        }
        let id = format!("rule:{index}");
        let conditions = clause
            .body
            .iter()
            .map(|literal| match literal {
                Lit::Pos(a) | Lit::Neg(a) => {
                    let negative = matches!(literal, Lit::Neg(_));
                    dependencies.push(json!({"source":a.pred,"target":clause.head.pred,"rule":id,
                    "polarity":if negative {"negative"} else {"positive"}}));
                    graph
                        .entry(a.pred.clone())
                        .or_default()
                        .insert(clause.head.pred.clone());
                    if negative {
                        Condition::Negative {
                            atom: atom(a, &types),
                        }
                    } else {
                        Condition::Positive {
                            atom: atom(a, &types),
                        }
                    }
                }
                Lit::Cmp(op, left, Expr::T(right)) => Condition::Comparison {
                    operator: match op {
                        CmpOp::Lt => "<",
                        CmpOp::Le => "<=",
                        CmpOp::Gt => ">",
                        CmpOp::Ge => ">=",
                        CmpOp::Eq => "=",
                        CmpOp::Ne => "!=",
                    }
                    .into(),
                    left: argument(left, &types),
                    right: argument(right, &types),
                },
                Lit::Cmp(..) => Condition::Unsupported {
                    description: "Arithmetic comparison is parsed but unsupported by execution"
                        .into(),
                },
                Lit::Now(..) => Condition::Unsupported {
                    description: "Clock builtin is parsed but unsupported by execution".into(),
                },
            })
            .collect();
        rules.push(Rule {
            id,
            name: clause.name.clone(),
            is_fact: clause.is_fact,
            head: atom(&clause.head, &types),
            conditions,
            variables: types,
            source_start: spanned.start,
            source_end: spanned.end,
            source: program.rules[spanned.start..spanned.end].into(),
        });
    }
    let names: BTreeSet<_> = graph
        .keys()
        .cloned()
        .chain(graph.values().flatten().cloned())
        .collect();
    let reachable = |start: &str| {
        let mut seen = BTreeSet::new();
        let mut pending: Vec<_> = graph.get(start).into_iter().flatten().cloned().collect();
        while let Some(next) = pending.pop() {
            if seen.insert(next.clone()) {
                pending.extend(graph.get(&next).into_iter().flatten().cloned());
            }
        }
        seen
    };
    let closure: BTreeMap<_, _> = names.iter().map(|n| (n.clone(), reachable(n))).collect();
    let mut recursive = BTreeSet::new();
    for name in &names {
        if closure[name].contains(name) {
            recursive.insert(
                closure[name]
                    .iter()
                    .filter(|other| closure[*other].contains(name))
                    .cloned()
                    .collect::<Vec<_>>(),
            );
        }
    }
    let relations: Vec<_> = schemas
        .iter()
        .map(|(name, schema)| {
            let public = program
                .interface
                .as_ref()
                .is_none_or(|i| i.inputs.contains(name) || i.outputs.contains(name));
            json!({"name":name,"fields":schema.fields,"input":schema.input,"public":public})
        })
        .collect();
    diagnostics.sort();
    diagnostics.dedup();
    json!({"schema_version":1,"kind":"program","source_sha256":format!("{:x}",Sha256::digest(program.rules.as_bytes())),
        "supported":diagnostics.is_empty(),"diagnostics":diagnostics,"native_compilation_performed":false,
        "relations":relations,"rules":rules,"dependencies":dependencies,"recursive_groups":recursive,
        "operators":program.operators,"source":program.rules})
}

pub fn inspect_record(record: &ProcessorVersion) -> Value {
    let mut value = match &record.definition {
        ProcessorDefinition::Program(program) => inspect_program(program),
        ProcessorDefinition::Composition(program) => {
            json!({"schema_version":1,"kind":"composition",
            "supported":true,"diagnostics":[],"native_compilation_performed":false,
            "composition":program.composition,"resolution":record.composition})
        }
    };
    value["processor"] = json!({"processor_id":record.processor_id,"version":record.version});
    value["provenance"] = json!(record.git_provenance);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    fn program(source: &str) -> ProgramDefinition {
        serde_json::from_value(json!({"rules":source,"schemas":{
            "edge":{"input":true,"fields":["int","int"]},
            "reach":{"input":false,"fields":["int","int"]},
            "missing":{"input":false,"fields":["int","int"]}}}))
        .unwrap()
    }
    #[test]
    fn inspection_preserves_logic_types_negation_recursion_and_utf8_spans() {
        let p = program("# λ comment\nreach(X,Y) :- edge(X,Y).\nreach(X,Z) :- reach(X,Y), edge(Y,Z), X != Z.\nmissing(X,Y) :- edge(X,Y), !reach(Y,X).");
        // The authored spelling for inequality is backslash-equals.
        let p = ProgramDefinition {
            rules: p.rules.replace("!=", "\\="),
            ..p
        };
        let result = inspect_program(&p);
        assert_eq!(result["supported"], true, "{result}");
        assert_eq!(result["rules"][1]["variables"]["Y"], "int");
        assert_eq!(result["rules"][1]["conditions"][2]["kind"], "comparison");
        assert_eq!(result["rules"][2]["conditions"][1]["kind"], "negative");
        assert_eq!(result["recursive_groups"], json!([["reach"]]));
        for rule in result["rules"].as_array().unwrap() {
            assert_eq!(
                &p.rules[rule["source_start"].as_u64().unwrap() as usize
                    ..rule["source_end"].as_u64().unwrap() as usize],
                rule["source"].as_str().unwrap()
            );
        }
    }
    #[test]
    fn parsed_facts_and_unsupported_constructs_are_never_claimed_executable() {
        for source in [
            "reach(1,2).",
            "reach(X,Y) :- edge(X,Y), Y = X + 1.",
            "reach(X,Y) :- edge(X,Y), now(Y).",
            "reach(X,Y) :- edge(X,Y), !reach(X,Y).",
        ] {
            let result = inspect_program(&program(source));
            assert_eq!(result["supported"], false, "{result}");
            assert!(!result["diagnostics"].as_array().unwrap().is_empty());
            assert_eq!(result["native_compilation_performed"], false);
            assert!(!result["rules"].as_array().unwrap().is_empty());
        }
        assert_eq!(
            inspect_program(&program("reach(1,2)."))["rules"][0]["is_fact"],
            true
        );
    }

    #[test]
    fn names_constants_wildcards_and_invalid_source_remain_truthful() {
        let p = program("# @diagram is an uninterpreted comment\nseed: reach(X,2) :- edge(X,_), edge(X,2), X >= -1.");
        let model = inspect_program(&p);
        assert_eq!(model["supported"], true, "{model}");
        let rule = &model["rules"][0];
        assert_eq!(rule["name"], "seed");
        assert_eq!(
            rule["head"]["arguments"][1],
            json!({"kind":"integer","value":2})
        );
        assert_eq!(
            rule["conditions"][0]["atom"]["arguments"][1]["kind"],
            "wildcard"
        );
        assert_eq!(rule["conditions"][2]["right"]["value"], -1);
        assert!(rule["source"].as_str().unwrap().starts_with("seed:"));
        assert_eq!(
            model["source_sha256"],
            format!("{:x}", Sha256::digest(p.rules.as_bytes()))
        );
        let bad = inspect_program(&program("reach(X,Y) :- edge(X,Y) ???"));
        assert_eq!(bad["supported"], false);
        assert_eq!(bad["rules"], json!([]));
    }

    #[test]
    fn declared_interface_and_opaque_native_extensions_are_preserved() {
        let p: ProgramDefinition = serde_json::from_value(json!({"rules":"", "schemas":{
            "vertices":{"input":true,"fields":["int"]},
            "edges":{"input":true,"fields":["int","int"]},
            "labels":{"input":false,"fields":["int","int"]}},
            "interface":{"inputs":["vertices","edges"],"outputs":["labels"]},
            "operators":[{"type":"large_small_star","vertices":"vertices","edges":"edges","output":"labels"}]})).unwrap();
        let result = inspect_program(&p);
        assert_eq!(result["supported"], true, "{result}");
        assert_eq!(result["rules"], json!([]));
        assert_eq!(result["operators"][0]["type"], "large_small_star");
        assert!(result["relations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["public"] == true));
    }
}
