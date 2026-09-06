//! Bounded transport reads from a single maintained native output relation.
//! The native CLI has no limit/cursor command: every page drains that relation's
//! dump. Selection here only compares decoded columns, never evaluates rules.
use crate::{rows, Backend, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_QUERY_ROWS: usize = 10_000;
pub const MAX_QUERY_BYTES: usize = 4 * 1024 * 1024;
/// Maximum native record size, including its trailing newline.
pub const MAX_NATIVE_RECORD_BYTES: usize = 4 * 1024 * 1024;
static OWNER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn owner_identity() -> String {
    format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        OWNER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

/// A page request with exact, zero-based positional equality filters.
/// All filters must match. Limits apply after filtering, and `max_bytes` counts
/// the exact JSON encoding of the returned `rows`, including outer brackets.
/// Neither limit bounds native scan/drain time; the CLI dumps the full selected
/// relation on each request. Unrelated relations are never requested.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundedQuery {
    pub filters: BTreeMap<usize, Value>,
    pub max_rows: usize,
    pub max_bytes: usize,
    pub continuation: Option<QueryCursor>,
}

impl Default for BoundedQuery {
    fn default() -> Self {
        Self {
            filters: BTreeMap::new(),
            max_rows: 100,
            max_bytes: 64 * 1024,
            continuation: None,
        }
    }
}

/// Continuation in native relation order, bound to one live owner, revision,
/// predicate and filter set. It is invalid after a mutation, replacement or
/// checkpoint restore. Its encoding is opaque to callers, not an authorization
/// token. Page limits may be changed while continuing the same query.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryCursor {
    owner: String,
    revision: u64,
    predicate: String,
    filters: BTreeMap<usize, Value>,
    offset: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryPage {
    pub rows: Vec<Vec<Value>>,
    pub bytes: usize,
    /// True only after observing an additional matching row.
    pub truncated: bool,
    pub continuation: Option<QueryCursor>,
    pub revision: u64,
}

struct CountBytes(usize);
impl Write for CountBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Backend {
    /// Read a bounded page from one maintained output without accumulating its
    /// full snapshot. A row too large for an empty page, malformed native record,
    /// or over-limit native record returns an error after draining the response;
    /// the healthy owner and acknowledged inputs remain usable. An I/O/framing
    /// failure still disables the owner, as with other native commands.
    pub fn query_typed_bounded(
        &mut self,
        predicate: &str,
        query: &BoundedQuery,
    ) -> Result<QueryPage> {
        if query.max_rows == 0 || query.max_rows > MAX_QUERY_ROWS {
            return Err(format!("max_rows must be in 1..={MAX_QUERY_ROWS}"));
        }
        if query.max_bytes < 2 || query.max_bytes > MAX_QUERY_BYTES {
            return Err(format!("max_bytes must be in 2..={MAX_QUERY_BYTES}"));
        }
        let schema = self.schema.get(predicate).ok_or("Unknown relation")?;
        if schema.input {
            return Err("Bounded queries accept output relations only".into());
        }
        for (position, value) in &query.filters {
            let field = schema
                .fields
                .get(*position)
                .ok_or_else(|| format!("Filter position {position} exceeds relation arity"))?;
            match field.as_str() {
                "int" if value.as_i64().is_some() => (),
                "string" if value.as_str().is_some() => (),
                _ => return Err(format!("Filter position {position} requires {field}")),
            }
        }
        let runtime = self.runtime.as_mut().ok_or("Install a program first")?;
        let offset = if let Some(cursor) = &query.continuation {
            if cursor.owner != runtime.identity
                || cursor.revision != self.revision
                || cursor.predicate != predicate
                || cursor.filters != query.filters
            {
                return Err("Continuation does not match the live owner, revision or query".into());
            }
            cursor.offset
        } else {
            0
        };
        let mut page = QueryPage {
            rows: Vec::new(),
            bytes: 2,
            truncated: false,
            continuation: None,
            revision: self.revision,
        };
        let mut matched = 0_u64;
        let mut error = None;
        let exchange = runtime.exchange_stream(&format!("dump R_{predicate};"), |line| {
            // The native stream must still be drained once the page is full or
            // its consumer rejects a record. No derived rows are cached.
            if error.is_some() || page.truncated {
                return;
            }
            let mut accept = |line: Result<&str>| -> Result<()> {
                let mut decoded = rows::decode_rows(line?, predicate, &schema.fields)?;
                let Some(row) = decoded.pop() else {
                    return Ok(());
                };
                if !query
                    .filters
                    .iter()
                    .all(|(position, value)| row[*position] == *value)
                {
                    return Ok(());
                }
                matched = matched.checked_add(1).ok_or("Row offset exhausted")?;
                if matched <= offset {
                    return Ok(());
                }
                let mut count = CountBytes(0);
                serde_json::to_writer(&mut count, &row).map_err(|e| e.to_string())?;
                if page.rows.is_empty() && count.0 + 2 > query.max_bytes {
                    return Err(format!(
                        "Matching row requires {} JSON page bytes, exceeding max_bytes={}; increase the limit",
                        count.0 + 2, query.max_bytes
                    ));
                }
                let bytes = page.bytes + usize::from(!page.rows.is_empty()) + count.0;
                if page.rows.len() == query.max_rows || bytes > query.max_bytes {
                    page.truncated = true;
                    return Ok(());
                }
                page.bytes = bytes;
                page.rows.push(row);
                Ok(())
            };
            if let Err(reason) = accept(line) {
                error = Some(reason);
            }
        });
        if let Err(error) = exchange {
            self.runtime = None;
            self.failed = true;
            return Err(format!(
                "Runtime unavailable; reconcile outstanding work: {error}"
            ));
        }
        if let Some(error) = error {
            return Err(error);
        }
        if matched < offset {
            return Err("Continuation offset exceeds the selected relation".into());
        }
        if page.truncated {
            page.continuation = Some(QueryCursor {
                owner: runtime.identity.clone(),
                revision: self.revision,
                predicate: predicate.to_string(),
                filters: query.filters.clone(),
                offset: offset
                    .checked_add(page.rows.len() as u64)
                    .ok_or("Row offset exhausted")?,
            });
        }
        Ok(page)
    }
}
