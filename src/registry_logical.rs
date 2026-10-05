//! Retryable logical publication inside the existing registry and update lock.
//! Preparation owns a pin; a separate immutable acknowledgment exposes it.
use super::*;

const MAX_RECORD: u64 = 2 * 1024 * 1024;
const MAX_PROGRAMS: usize = 1024;
const MAX_INSPECTED_VERSIONS: usize = 16384;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LogicalProgramBinding {
    pub resource: String,
    pub request_key: String,
    pub request_sha256: String,
}
impl LogicalProgramBinding {
    pub(super) fn validate(&self) -> Result<()> {
        token(&self.resource)?;
        token(&self.request_key)?;
        if !is_hex(&self.request_sha256, 64) {
            return Err("Invalid logical program binding digest".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LogicalProgramRequest {
    pub request_key: String,
    pub resource: String,
    pub description: String,
    pub definition: ProcessorDefinition,
    pub git_provenance: Option<GitProvenance>,
    pub lowering_version: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LogicalProgram {
    pub resource: String,
    pub request_key: String,
    pub request_sha256: String,
    pub processor: ProcessorReference,
    /// `prepared` is reserved but not yet a publicly usable program binding.
    pub phase: String,
}

#[derive(Clone, Copy, Default)]
pub enum LogicalProgramFault {
    #[default]
    None,
    AfterPreparation,
    AfterVersion,
    AfterPointer,
    AfterPublication,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepared {
    schema_version: u32,
    request: LogicalProgramRequest,
    request_sha256: String,
    record: ProcessorVersion,
}

fn token(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
    {
        return Err("Invalid logical program identifier".into());
    }
    Ok(())
}
fn digest(value: &impl Serialize) -> Result<String> {
    // serde_json Value maps use canonical key order in this crate.
    let value = serde_json::to_value(value).map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn bounded<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let meta = fs::symlink_metadata(path).map_err(io_error)?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_RECORD {
        return Err("Invalid or oversized logical program record".into());
    }
    read_json(path)
}
fn fence(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(io_error)?;
    sync_directory(path.parent().ok_or("Missing record directory")?)
}
impl Prepared {
    fn value(&self, published: bool) -> LogicalProgram {
        LogicalProgram {
            resource: self.request.resource.clone(),
            request_key: self.request.request_key.clone(),
            request_sha256: self.request_sha256.clone(),
            processor: ProcessorReference {
                processor_id: self.record.processor_id.clone(),
                version: self.record.version.clone(),
            },
            phase: if published { "published" } else { "prepared" }.into(),
        }
    }
    fn validate(&self, registry: &ProcessorRegistry) -> Result<()> {
        token(&self.request.resource)?;
        token(&self.request.request_key)?;
        verify_envelope(
            &self.record,
            &self.record.processor_id,
            &self.record.version,
        )?;
        let composition = registry.validate_definition_versioned(
            &self.request.definition,
            self.request.lowering_version,
        )?;
        if self.schema_version != 1
            || self.request.description.len() > 4096
            || !matches!(self.request.lowering_version, 1 | 2)
            || self.request_sha256 != digest(&self.request)?
            || self.record.definition != self.request.definition
            || self.record.git_provenance != self.request.git_provenance
            || self.record.lineage.is_some()
            || self.record.composition != composition
            || self.record.logical_binding
                != Some(LogicalProgramBinding {
                    resource: self.request.resource.clone(),
                    request_key: self.request.request_key.clone(),
                    request_sha256: self.request_sha256.clone(),
                })
        {
            return Err("Logical program preparation mismatch".into());
        }
        Ok(())
    }
}
impl ProcessorRegistry {
    // Inspect canonical versions, including those whose first current pointer
    // was never published. Retain at most one bounded record at a time.
    fn check_logical_authority(&self, request: &LogicalProgramRequest) -> Result<()> {
        let mut directories = 0;
        let mut versions = 0;
        for entry in fs::read_dir(&self.root).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let id = entry.file_name().to_string_lossy().into_owned();
            if validate_processor_id(&id).is_err() {
                continue;
            }
            directories += 1;
            if directories > MAX_INSPECTED_VERSIONS {
                return Err("Logical program authority inspection limit".into());
            }
            check_private_dir(&entry.path())?;
            let directory = entry.path().join("versions");
            if !directory.try_exists().map_err(io_error)? {
                continue;
            }
            check_private_dir(&directory)?;
            for entry in fs::read_dir(directory).map_err(io_error)? {
                let entry = entry.map_err(io_error)?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(hex) = name.strip_suffix(".json").filter(|hex| is_hex(hex, 64)) else {
                    continue;
                };
                versions += 1;
                if versions > MAX_INSPECTED_VERSIONS {
                    return Err("Logical program authority inspection limit".into());
                }
                let record: ProcessorVersion = bounded(&entry.path())?;
                verify_envelope(&record, &id, &format!("sha256:{hex}"))?;
                if let Some(binding) = &record.logical_binding {
                    let prepared = self.read_logical(&binding.resource).map_err(|error| {
                        format!("Retained logical program has lost valid binding facts: {error}")
                    })?;
                    if prepared.record != record {
                        return Err(
                            "Retained logical program binding facts differ from canonical version"
                                .into(),
                        );
                    }
                    if binding.resource == request.resource
                        || binding.request_key == request.request_key
                    {
                        return Err(
                            "Logical program destination or request already retained".into()
                        );
                    }
                }
            }
        }
        Ok(())
    }
    fn logical_path(&self, resource: &str) -> Result<PathBuf> {
        token(resource)?;
        Ok(self
            .root
            .join("logical")
            .join(format!("{}.json", digest(&resource)?)))
    }
    fn read_logical(&self, resource: &str) -> Result<Prepared> {
        let prepared: Prepared = bounded(&self.logical_path(resource)?)?;
        prepared.validate(self)?;
        if prepared.request.resource != resource {
            return Err("Logical program destination mismatch".into());
        }
        Ok(prepared)
    }
    /// Cold exact identity, including an interrupted preparation. Never writes,
    /// advances a mutable pointer, activates code, or requires a request key.
    pub fn resolve_logical_program(&self, resource: &str) -> Result<LogicalProgram> {
        let prepared = self.read_logical(resource)?;
        let root = self.logical_path(resource)?.with_extension("published");
        if !root.try_exists().map_err(io_error)? {
            return Ok(prepared.value(false));
        }
        let value: LogicalProgram = bounded(&root)?;
        if value != prepared.value(true)
            || self.get(
                &value.processor.processor_id,
                Some(&value.processor.version),
            )? != prepared.record
        {
            return Err("Published logical program differs from retained preparation".into());
        }
        Ok(value)
    }
    pub fn publish_logical_program(
        &self,
        request: LogicalProgramRequest,
    ) -> Result<LogicalProgram> {
        self.publish_logical_program_with_fault(request, LogicalProgramFault::None)
    }
    pub fn publish_logical_program_with_fault(
        &self,
        request: LogicalProgramRequest,
        fault: LogicalProgramFault,
    ) -> Result<LogicalProgram> {
        token(&request.resource)?;
        token(&request.request_key)?;
        if request.description.len() > 4096
            || !matches!(request.lowering_version, 1 | 2)
            || serde_json::to_vec(&request)
                .map_err(|e| e.to_string())?
                .len() as u64
                > MAX_RECORD / 2
        {
            return Err("Logical program request exceeds limit".into());
        }
        let composition =
            self.validate_definition_versioned(&request.definition, request.lowering_version)?;
        let _lock = UpdateLock::acquire(&self.root)?;
        let path = self.logical_path(&request.resource)?;
        let root = path.with_extension("published");
        let prepared = if path.try_exists().map_err(io_error)? {
            let prior = self.read_logical(&request.resource)?;
            if prior.request != request {
                return Err("Logical program destination already binds another request".into());
            }
            prior
        } else {
            if root.try_exists().map_err(io_error)? {
                return Err("Published logical program has lost its preparation".into());
            }
            self.check_logical_authority(&request)?;
            self.ensure_references_active(composition.as_ref())?;
            let directory = path.parent().unwrap();
            if directory.exists() {
                check_private_dir(directory)?;
                let mut count = 0;
                for entry in fs::read_dir(directory).map_err(io_error)? {
                    let entry = entry.map_err(io_error)?.path();
                    if entry.extension().and_then(|s| s.to_str()) != Some("json") {
                        continue;
                    }
                    count += 1;
                    if count >= MAX_PROGRAMS {
                        return Err("Logical program catalog limit".into());
                    }
                    let prior: Prepared = bounded(&entry)?;
                    prior.validate(self)?;
                    if prior.request.request_key == request.request_key {
                        return Err(
                            "Logical program request key already names another destination".into(),
                        );
                    }
                }
            }
            let content_sha256 = definition_hash(&request.definition)?;
            let prepared = Prepared {
                schema_version: 1,
                request_sha256: digest(&request)?,
                record: ProcessorVersion {
                    format_version: FORMAT_VERSION,
                    processor_id: format!("processor_{}", random_hex()?),
                    version: format!("sha256:{content_sha256}"),
                    content_sha256,
                    created_at_unix_ms: now_unix_ms()?,
                    git_provenance: request.git_provenance.clone(),
                    lineage: None,
                    definition: request.definition.clone(),
                    validation: DefinitionValidation::checked(),
                    composition,
                    logical_binding: Some(LogicalProgramBinding {
                        resource: request.resource.clone(),
                        request_key: request.request_key.clone(),
                        request_sha256: digest(&request)?,
                    }),
                },
                request,
            };
            if serde_json::to_vec_pretty(&prepared)
                .map_err(|e| e.to_string())?
                .len() as u64
                > MAX_RECORD
            {
                return Err("Logical program preparation exceeds limit".into());
            }
            create_private_dir(directory)?;
            check_private_dir(directory)?;
            atomic_json(&path, &prepared, false)?;
            prepared
        };
        sync_directory(&self.root)?;
        fence(&path)?;
        if matches!(fault, LogicalProgramFault::AfterPreparation) {
            return Err("Injected failure after logical program preparation".into());
        }
        if root.try_exists().map_err(io_error)? {
            let resolved = self.resolve_logical_program(&prepared.request.resource)?;
            fence(&root)?;
            return Ok(resolved);
        }
        let record = &prepared.record;
        let directory = self.root.join(&record.processor_id);
        create_private_dir(&directory)?;
        check_private_dir(&directory)?;
        sync_directory(&self.root)?;
        create_private_dir(&directory.join("versions"))?;
        sync_directory(&directory)?;
        let version_path = self.version_path(&record.processor_id, &record.version);
        if version_path.try_exists().map_err(io_error)? {
            if self.get(&record.processor_id, Some(&record.version))? != *record {
                return Err("Reserved logical program version changed".into());
            }
        } else {
            atomic_json(&version_path, record, false)?;
        }
        fence(&version_path)?;
        if matches!(fault, LogicalProgramFault::AfterVersion) {
            return Err("Injected failure after logical program version".into());
        }
        let current_path = directory.join("current.json");
        if current_path.try_exists().map_err(io_error)? {
            // A later ordinary conditional publication may have advanced it.
            // Verify that pointer but never roll it back during exact retry.
            let current = self.current(&record.processor_id)?;
            let selected = self.get(&record.processor_id, Some(&current.version))?;
            if selected.lineage != current.lineage {
                return Err("Logical program current pointer lineage mismatch".into());
            }
        } else {
            atomic_json(
                &current_path,
                &Current {
                    format_version: FORMAT_VERSION,
                    processor_id: record.processor_id.clone(),
                    version: record.version.clone(),
                    lineage: None,
                },
                false,
            )?;
        }
        fence(&current_path)?;
        if matches!(fault, LogicalProgramFault::AfterPointer) {
            return Err("Injected failure after logical program pointer".into());
        }
        if self.get(&record.processor_id, Some(&record.version))? != *record {
            return Err("Logical program readback mismatch".into());
        }
        let value = prepared.value(true);
        atomic_json(&root, &value, false)?;
        if matches!(fault, LogicalProgramFault::AfterPublication) {
            return Err("Injected failure after logical program publication".into());
        }
        self.resolve_logical_program(&prepared.request.resource)
    }
}
