//! Shared rule syntax, independent of any evaluator or transport.
pub mod ast;
pub use ast::{parse_program, Atom, Clause, CmpOp, Expr, Lit, ParseError};

/// Aggregate functions usable in rule HEAD arguments only
/// (`kit_count(P, count(K))`). Lowered internally to a temp relation plus
/// a group-by fold; the head predicate completes before any reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFn {
    Count,
    Min,
    Max,
    Sum,
}

impl AggFn {
    pub fn name(&self) -> &'static str {
        match self {
            AggFn::Count => "count",
            AggFn::Min => "min",
            AggFn::Max => "max",
            AggFn::Sum => "sum",
        }
    }
}

/// A term appearing in rules: a variable, a constant symbol, or an integer.
#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    Var(String),
    Sym(String),
    Int(i64),
    Wildcard,
    /// Aggregated head argument: `count(X)`, `min(D)`, ... (heads only)
    Agg(AggFn, Box<Term>),
}

impl Term {
    pub fn render(&self) -> String {
        match self {
            Term::Var(v) => v.clone(),
            Term::Sym(s) => format!("\"{s}\""),
            Term::Int(i) => i.to_string(),
            Term::Wildcard => "_".to_string(),
            Term::Agg(f, t) => format!("{}({})", f.name(), t.render()),
        }
    }
}
