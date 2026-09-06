//! Explicit local checkpoints of acknowledged pure-program state.
//! No WAL, automatic recovery, provider retry or external-effect restoration.
use crate::{Backend, Result, Schema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const FORMAT_VERSION: u32 = 1;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    format_version: u32,
    source: String,
    schemas: BTreeMap<String, Schema>,
    inputs: BTreeMap<String, Vec<Vec<Value>>>,
    revision: u64,
    program_version: u64,
    /// Opaque application payload, integrity-bound but never interpreted by runtime.
    metadata: Value,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    sha256: String,
    state: State,
}
fn digest(state: &State) -> Result<String> {
    let canonical = serde_json::to_value(state).map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec(&canonical).map_err(|e| e.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn decode_checkpoint(bytes: &[u8]) -> Result<Checkpoint> {
    let checkpoint: Checkpoint = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if digest(&checkpoint.state)? != checkpoint.sha256 {
        return Err("Checkpoint integrity digest mismatch".into());
    }
    Ok(checkpoint)
}
fn pure(schemas: &BTreeMap<String, Schema>) -> Result<()> {
    if schemas.keys().any(|name| name.starts_with("agent_")) {
        return Err("Registered-operation state cannot be checkpointed or restored; external outcomes require reconciliation".into());
    }
    Ok(())
}
impl State {
    fn validate(&self) -> Result<BTreeMap<(String, String), Vec<Value>>> {
        if self.format_version != FORMAT_VERSION {
            return Err("Unsupported checkpoint format".into());
        }
        if self.program_version == 0
            || self.revision < self.program_version
            || self.source.is_empty()
        {
            return Err("Invalid checkpoint program identity".into());
        }
        pure(&self.schemas)?;
        // Generated imports reference implementation files outside this snapshot.
        // Until those exact files are integrity-bound too, restoration could
        // silently substitute a different native implementation.
        if self
            .source
            .lines()
            .any(|line| line.trim_start().starts_with("import "))
        {
            return Err("Checkpoint format 1 does not support imported native operators".into());
        }
        let input_names: BTreeSet<_> = self
            .schemas
            .iter()
            .filter(|(_, s)| s.input)
            .map(|(name, _)| name)
            .collect();
        if self.inputs.keys().collect::<BTreeSet<_>>() != input_names {
            return Err(
                "Checkpoint input relations do not match schema (including empty relations)".into(),
            );
        }
        let mut declarations = BTreeSet::new();
        for (name, schema) in &self.schemas {
            if !crate::lower::ident(name) || schema.fields.is_empty() {
                return Err("Invalid checkpoint schema".into());
            }
            let mut fields = Vec::new();
            for (i, field) in schema.fields.iter().enumerate() {
                let ty = match field.as_str() {
                    "int" => "signed<64>",
                    "string" => "string",
                    _ => return Err("Unsupported checkpoint field type".into()),
                };
                fields.push(format!("f{i}: {ty}"));
            }
            declarations.insert(format!(
                "{} relation R_{name}({})",
                if schema.input { "input" } else { "output" },
                fields.join(", ")
            ));
        }
        let actual: Vec<_> = self
            .source
            .lines()
            .filter(|line| {
                line.starts_with("input relation R_") || line.starts_with("output relation R_")
            })
            .collect();
        if actual.len() != declarations.len()
            || actual
                .into_iter()
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
                != declarations
        {
            return Err("Checkpoint generated source declarations do not match schemas".into());
        }
        let mut validator = Backend::new("unused".into(), "unused".into());
        validator.schema = self.schemas.clone();
        let mut facts = BTreeMap::new();
        for (name, rows) in &self.inputs {
            for row in rows {
                let fact = validator.fact(name, row)?;
                if facts.insert((name.clone(), fact), row.clone()).is_some() {
                    return Err("Duplicate checkpoint input fact".into());
                }
            }
        }
        Ok(facts)
    }
}

impl Backend {
    /// Atomically publish a local checkpoint. The digest detects corruption;
    /// it is not authentication. Paths and checkpoints are operator-controlled.
    /// The metadata is opaque and is returned unchanged by restore.
    pub fn save_checkpoint(&self, path: &Path, metadata: Value) -> Result<Value> {
        pure(&self.schema)?;
        let state = State {
            format_version: FORMAT_VERSION,
            source: self.active_source.clone(),
            schemas: self.schema.clone(),
            inputs: self.export_inputs()?,
            revision: self.revision,
            program_version: self.version,
            metadata,
        };
        state.validate()?;
        let sha256 = digest(&state)?;
        let bytes = serde_json::to_vec(&Checkpoint {
            sha256: sha256.clone(),
            state,
        })
        .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("Checkpoint exceeds 64 MiB limit".into());
        }
        // Opaque metadata must survive the same parser and digest verification
        // as restore, including its recursion limit, before any file is touched.
        decode_checkpoint(&bytes)?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let filename = path
            .file_name()
            .ok_or("Checkpoint requires a file path")?
            .to_string_lossy();
        let temp = parent.join(format!(
            ".{filename}.{}.{}.tmp",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|e| e.to_string())?;
        let result = (|| -> std::io::Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temp, path)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temp);
            return Err(format!(
                "Checkpoint publication failed (reconcile target if rename completed): {error}"
            ));
        }
        Ok(json!({"format_version":FORMAT_VERSION,"sha256":sha256,"revision":self.revision}))
    }

    /// Restore into a fresh backend. Verify integrity and typed inputs before
    /// compilation; activate only after successful native replay. Returns the
    /// opaque application metadata. Completed provider calls are not replayed.
    pub fn restore_checkpoint(&mut self, path: &Path) -> Result<Value> {
        if self.health() != "uninitialized" || self.version != 0 || !self.facts.is_empty() {
            return Err("Checkpoint restore requires a fresh uninitialized backend".into());
        }
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|e| e.to_string())?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("Checkpoint exceeds 64 MiB limit".into());
        }
        let checkpoint = decode_checkpoint(&bytes)?;
        let facts = checkpoint.state.validate()?;
        // Stage in a separate owner; errors leave this backend untouched.
        let mut candidate = Backend::new(self.root.clone(), self.driver.clone());
        candidate.attempt = self.attempt;
        candidate.facts = facts;
        candidate.schema = checkpoint.state.schemas.clone();
        candidate.install_source(checkpoint.state.source, checkpoint.state.schemas)?;
        candidate.revision = checkpoint.state.revision;
        candidate.version = checkpoint.state.program_version;
        *self = candidate;
        Ok(checkpoint.state.metadata)
    }
}
