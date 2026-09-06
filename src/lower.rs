use super::star::Operator;
use lemmalog_syntax::ast::{parse_program, Atom, Clause, CmpOp, Expr, Lit};
use lemmalog_syntax::Term;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    pub input: bool,
    pub fields: Vec<String>,
}
pub(super) fn ident(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && s.as_bytes()[0].is_ascii_alphabetic()
}
pub(super) fn string_literal(s: &str) -> Result<String, String> {
    if s.chars()
        .any(|c| c < ' ' && !matches!(c, '\x08' | '\t' | '\n' | '\x0c' | '\r'))
    {
        return Err("Unsupported control character for the DDlog CLI".into());
    }
    serde_json::to_string(s).map_err(|e| e.to_string())
}
fn term(t: &Term) -> Result<String, String> {
    match t {
        Term::Var(v) if ident(v) => Ok(format!("v_{v}")),
        Term::Sym(s) => string_literal(s),
        Term::Int(i) => Ok(i.to_string()),
        Term::Wildcard => Ok("_".into()),
        _ => Err("Unsupported term (aggregates are not supported)".into()),
    }
}
fn atom(a: &Atom) -> Result<String, String> {
    Ok(format!(
        "R_{}({})",
        a.pred,
        a.args
            .iter()
            .map(term)
            .collect::<Result<Vec<_>, _>>()?
            .join(", ")
    ))
}
#[derive(Clone, Copy)]
enum AtomPosition {
    Positive,
    Negative,
    Head,
}

fn check(
    a: &Atom,
    schemas: &BTreeMap<String, Schema>,
    vars: &mut BTreeMap<String, String>,
    position: AtomPosition,
) -> Result<(), String> {
    let schema = schemas
        .get(&a.pred)
        .ok_or_else(|| format!("Undeclared relation {}", a.pred))?;
    if a.args.len() != schema.fields.len() {
        return Err(format!(
            "Arity mismatch for {}: expected {} fields {:?}, got {}",
            a.pred,
            schema.fields.len(),
            schema.fields,
            a.args.len()
        ));
    }
    for (index, (t, kind)) in a.args.iter().zip(&schema.fields).enumerate() {
        match t {
            Term::Var(v) => {
                if let Some(previous) = vars.get(v) {
                    if previous != kind {
                        return Err(format!("Conflicting types for {v} at {} field {index}: previously {previous}, requires {kind}", a.pred));
                    }
                } else if matches!(position, AtomPosition::Positive) {
                    vars.insert(v.clone(), kind.clone());
                } else {
                    let location = match position {
                        AtomPosition::Negative => "negated",
                        _ => "head",
                    };
                    return Err(format!("Unbound {location} variable {v} in {}", a.pred));
                }
            }
            Term::Int(_) if kind == "int" => {}
            Term::Sym(_) if kind == "string" => {}
            Term::Wildcard if !matches!(position, AtomPosition::Head) => {}
            _ => {
                return Err(format!(
                    "Unsupported or mismatched term {t:?} at {} field {index}: requires {kind}",
                    a.pred
                ))
            }
        }
    }
    Ok(())
}
/// Lower typed rules with positive recursion and stratified negation.
/// Unsupported constructs fail before the installed program is touched.
pub fn lower(rules: &str, schemas: &BTreeMap<String, Schema>) -> Result<String, String> {
    lower_with_operators(rules, schemas, &[])
}

pub fn lower_with_operators(
    rules: &str,
    schemas: &BTreeMap<String, Schema>,
    operators: &[Operator],
) -> Result<String, String> {
    let clauses = parse_program(rules).map_err(|e| e.to_string())?;
    lower_clauses_with_operators(&clauses, schemas, operators)
}

/// Composition renames predicates and typed operators without round-tripping
/// authored terms or string literals through a second source parser.
pub(super) fn lower_clauses_with_operators(
    clauses: &[Clause],
    schemas: &BTreeMap<String, Schema>,
    operators: &[Operator],
) -> Result<String, String> {
    if clauses.is_empty() && operators.is_empty() {
        return Err("Expected at least one rule".into());
    }
    let mut out = String::new();
    if !operators.is_empty() {
        out.push_str(&super::star::prelude());
    }
    for (name, s) in schemas {
        if !ident(name) || s.fields.is_empty() {
            return Err("Invalid relation name or zero arity".into());
        }
        let fields = s
            .fields
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let ty = match t.as_str() {
                    "int" => "signed<64>",
                    "string" => "string",
                    _ => return Err("Only int and string fields are supported".to_string()),
                };
                Ok(format!("f{i}: {ty}"))
            })
            .collect::<Result<Vec<_>, String>>()?;
        out.push_str(&format!(
            "{} relation R_{name}({})\n",
            if s.input { "input" } else { "output" },
            fields.join(", ")
        ));
    }
    let mut dependencies: BTreeMap<String, Vec<(String, DependencyKind)>> = BTreeMap::new();
    for (index, operator) in operators.iter().enumerate() {
        operator.validate(schemas)?;
        let (vertices, edges, output) = operator.relations();
        dependencies.entry(output.into()).or_default().extend([
            (vertices.into(), DependencyKind::Native),
            (edges.into(), DependencyKind::Native),
        ]);
        out.push_str(&operator.source(index));
    }
    for (index, c) in clauses.iter().enumerate() {
        if c.is_fact {
            return Err("Facts must be submitted through apply_changes".into());
        }
        if schemas
            .get(&c.head.pred)
            .ok_or_else(|| format!("Undeclared head relation {}", c.head.pred))?
            .input
        {
            return Err(format!(
                "Rule head {} must be an output relation",
                c.head.pred
            ));
        }
        let mut vars = BTreeMap::new();
        for lit in &c.body {
            if let Lit::Pos(a) = lit {
                check(a, schemas, &mut vars, AtomPosition::Positive)?;
                dependencies
                    .entry(c.head.pred.clone())
                    .or_default()
                    .push((a.pred.clone(), DependencyKind::Positive));
            }
        }
        check(&c.head, schemas, &mut vars, AtomPosition::Head)?;
        let mut body = Vec::new();
        let mut negative_body = Vec::new();
        for lit in &c.body {
            if let Lit::Neg(a) = lit {
                check(a, schemas, &mut vars, AtomPosition::Negative)?;
                dependencies
                    .entry(c.head.pred.clone())
                    .or_default()
                    .push((a.pred.clone(), DependencyKind::Negative));
                // DDlog requires variables bound before an antijoin. Moving
                // negated atoms after the positive body also accepts authored
                // negation preceding its positive binder, without changing
                // generated source for previously supported programs.
                negative_body.push(format!("not {}", atom(a)?));
                continue;
            }
            body.push(match lit {
                Lit::Pos(a) => atom(a)?,
                Lit::Cmp(op, left, Expr::T(right)) => {
                    let kind = |t: &Term| match t {
                        Term::Int(_) => Ok("int"),
                        Term::Sym(_) => Ok("string"),
                        Term::Var(v) => vars
                            .get(v)
                            .map(String::as_str)
                            .ok_or("Unbound comparison variable"),
                        _ => Err("Unsupported comparison term"),
                    };
                    if kind(left)? != kind(right)? {
                        return Err("Comparison type mismatch".into());
                    }
                    // Lemmalog ordering comparisons are numeric; do not introduce string ordering.
                    if !matches!(op, CmpOp::Eq | CmpOp::Ne) && kind(left)? != "int" {
                        return Err("Ordering requires integers".into());
                    }
                    let op = match op {
                        CmpOp::Lt => "<",
                        CmpOp::Le => "<=",
                        CmpOp::Gt => ">",
                        CmpOp::Ge => ">=",
                        CmpOp::Eq => "==",
                        CmpOp::Ne => "!=",
                    };
                    format!("{} {op} {}", term(left)?, term(right)?)
                }
                _ => {
                    return Err(
                        "Aggregates, arithmetic and clock builtins are not supported".into(),
                    )
                }
            });
        }
        body.extend(negative_body);
        if vars.is_empty() {
            return Err("Rule must bind at least one variable".into());
        }
        let fields = vars
            .iter()
            .map(|(v, t)| {
                format!(
                    "v_{v}: {}",
                    if t == "int" { "signed<64>" } else { "string" }
                )
            })
            .collect::<Vec<_>>();
        let names = vars.keys().map(|v| format!("v_{v}")).collect::<Vec<_>>();
        out.push_str(&format!(
            "output relation Evidence{index}({})\n",
            fields.join(", ")
        ));
        out.push_str(&format!(
            "Evidence{index}({}) :- {}.\n",
            names.join(", "),
            body.join(", ")
        ));
        out.push_str(&format!(
            "{} :- Evidence{index}({}).\n",
            atom(&c.head)?,
            names.join(", ")
        ));
    }
    validate_dependencies(&dependencies)?;
    Ok(out)
}

#[derive(Clone, Copy)]
enum DependencyKind {
    Positive,
    Negative,
    Native,
}

/// A negative edge must not belong to any dependency cycle. Pure positive
/// cycles are finite-domain DDlog least fixpoints: authored arithmetic and
/// term constructors remain unsupported. Native transformers are not ordinary
/// monotone rule bodies, so cycles through their inputs remain unsupported.
fn validate_dependencies(
    graph: &BTreeMap<String, Vec<(String, DependencyKind)>>,
) -> Result<(), String> {
    let reaches = |start: &str, target: &str| {
        let mut pending = vec![start];
        let mut visited = BTreeSet::new();
        while let Some(node) = pending.pop() {
            if node == target {
                return true;
            }
            if !visited.insert(node) {
                continue;
            }
            if let Some(edges) = graph.get(node) {
                pending.extend(edges.iter().map(|(next, _)| next.as_str()));
            }
        }
        false
    };
    for (head, edges) in graph {
        for (body, kind) in edges {
            if matches!(kind, DependencyKind::Positive) || !reaches(body, head) {
                continue;
            }
            return Err(match kind {
                DependencyKind::Negative => format!(
                    "Unstratified negation: {head} negatively depends on {body} in a dependency cycle"
                ),
                DependencyKind::Native => format!(
                    "Recursive dependency through native operator: {head} depends on {body}"
                ),
                DependencyKind::Positive => unreachable!(),
            });
        }
    }
    Ok(())
}
