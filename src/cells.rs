//! Exact live-cell validation. Zero normalization precedes retained identities.
use serde_json::Value;

pub(crate) fn matches(value: &Value, field: &str) -> bool {
    match field {
        "int" => value.as_i64().is_some(),
        "string" => value.is_string(),
        "bool" => value.is_boolean(),
        "double" => value.is_f64() && value.as_f64().is_some_and(f64::is_finite),
        _ => false,
    }
}
pub(crate) fn normalize(value: &mut Value) {
    if value.is_f64() && value.as_f64() == Some(0.0) {
        *value = Value::from(0.0);
    }
}
pub(crate) fn row(fields: &[String], values: &[Value]) -> Result<Vec<Value>, String> {
    if fields.len() != values.len() {
        return Err("Arity mismatch".into());
    }
    values
        .iter()
        .zip(fields)
        .map(|(value, field)| {
            if !matches(value, field) {
                return Err(format!("Expected exact {field} cell"));
            }
            let mut value = value.clone();
            normalize(&mut value);
            Ok(value)
        })
        .collect()
}
