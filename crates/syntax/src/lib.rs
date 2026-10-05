//! Shared rule syntax, independent of any evaluator or transport.
pub mod ast;
pub use ast::{parse_program, Atom, Clause, CmpOp, Expr, Lit, ParseError};

/// Aggregate syntax recognized in rule heads, such as `kit_count(P, count(K))`.
/// This crate only parses the syntax; each consumer decides which constructs it
/// supports. DDlog Runtime currently rejects aggregates during lowering.
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
    Bool(bool),
    Float(f64),
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
            Term::Bool(value) => value.to_string(),
            Term::Float(value) => float_text(*value).unwrap_or_else(|_| value.to_string()),
            Term::Wildcard => "_".to_string(),
            Term::Agg(f, t) => format!("{}({})", f.name(), t.render()),
        }
    }
}

/// Strict finite decimal binary64, also used for schema-directed native output.
/// Reject overflow and nonzero values rounded down to zero; normalize both zeros.
pub fn parse_float(text: &str) -> Result<f64, String> {
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(index) => (&unsigned[..index], Some(&unsigned[index + 1..])),
        None => (unsigned, None),
    };
    let mut parts = mantissa.split('.');
    let whole = parts.next().unwrap_or("");
    let fraction = parts.next();
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.is_some_and(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
        || parts.next().is_some()
        || exponent.is_some_and(|s| {
            let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
            digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit())
        })
    {
        return Err("Expected finite decimal Float64".into());
    }
    let value: f64 = text.parse().map_err(|_| "Invalid Float64 decimal")?;
    if !value.is_finite() || (value == 0.0 && mantissa.bytes().any(|b| matches!(b, b'1'..=b'9'))) {
        return Err("Float64 overflow, underflow or nonfinite value".into());
    }
    Ok(if value == 0.0 { 0.0 } else { value })
}

/// Roundtripping decimal with a point, suitable for native double records.
pub fn float_text(value: f64) -> Result<String, String> {
    if !value.is_finite() {
        return Err("Float64 must be finite".into());
    }
    let value = if value == 0.0 { 0.0 } else { value };
    let mut text = format!("{value:?}");
    if !text.contains('.') {
        let end = text.find(['e', 'E']).unwrap_or(text.len());
        text.insert_str(end, ".0");
    }
    Ok(text)
}
