//! Durable, same-user processor definitions. Registry versions contain no live facts,
//! requests, workers, or runtime state. A host pins a returned immutable version.
//!
//! Writers serialize through a create-new lock file. A crashed writer can leave the
//! lock behind: only an operator who has established writer absence may remove it.
//! Readers never follow a mutable pointer twice when selecting one version.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::composition::{
    CompiledComposition, CompositionManifest, CompositionResolution, ProgramInterface, ResolvedNode,
};

pub type Result<T> = std::result::Result<T, String>;
const FORMAT_VERSION: u32 = 1;

/// Exact registered operation selected when the definition was authored. A host
/// must compare all fields to its trusted operation registry before installing it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RegisteredOperationBinding {
    pub name: String,
    pub version: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ProcessorDefinition {
    Program(ProgramDefinition),
    Composition(CompositionDefinition),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProgramDefinition {
    pub rules: String,
    pub schemas: Value,
    #[serde(default)]
    pub operation: Option<RegisteredOperationBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<ProgramInterface>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operators: Vec<super::star::Operator>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CompositionDefinition {
    pub composition: CompositionManifest,
}

/// Optional operator-supplied provenance, not an identity or integrity claim.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitProvenance {
    pub repository: String,
    pub revision: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProcessorReference {
    pub processor_id: String,
    pub version: String,
}

/// Checks completed before publication. These describe pure lowering only;
/// successful native DDlog compilation is still required at installation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DefinitionValidation {
    pub syntax_checked: bool,
    pub types_checked: bool,
    pub supported_lowering_checked: bool,
    pub ddlog_compilation_performed: bool,
}
impl DefinitionValidation {
    fn checked() -> Self {
        Self {
            syntax_checked: true,
            types_checked: true,
            supported_lowering_checked: true,
            ddlog_compilation_performed: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProcessorVersion {
    pub format_version: u32,
    pub processor_id: String,
    /// Content-addressed identifier, distinct from the processor's stable identity.
    pub version: String,
    pub content_sha256: String,
    pub created_at_unix_ms: u64,
    pub git_provenance: Option<GitProvenance>,
    /// The exact source of a fork, retained on every version of the new processor.
    pub lineage: Option<ProcessorReference>,
    pub definition: ProcessorDefinition,
    pub validation: DefinitionValidation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<CompositionResolution>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessorStatus {
    Active,
    Archived,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessorKind {
    Program,
    Composition,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProcessorLifecycle {
    pub format_version: u32,
    pub processor_id: String,
    pub version: String,
    pub status: ProcessorStatus,
    pub lifecycle_revision: u64,
    /// None until the first lifecycle transition; definition publication has no
    /// effect on this clock or on lifecycle_revision.
    pub changed_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProcessorSummary {
    pub processor_id: String,
    pub version: String,
    pub content_sha256: String,
    pub created_at_unix_ms: u64,
    pub kind: ProcessorKind,
    pub status: ProcessorStatus,
    pub lifecycle_revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at_unix_ms: Option<u64>,
    pub lineage: Option<ProcessorReference>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProcessorPage {
    pub processors: Vec<ProcessorSummary>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Current {
    format_version: u32,
    processor_id: String,
    version: String,
    lineage: Option<ProcessorReference>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LifecyclePointer {
    format_version: u32,
    processor_id: String,
    lifecycle_revision: u64,
}

#[derive(Clone, Debug)]
pub struct ProcessorRegistry {
    root: PathBuf,
}

impl ProcessorRegistry {
    /// Open or create a private registry directory. Existing public directories
    /// are rejected; callers should select a dedicated same-user directory.
    pub fn open(root: PathBuf) -> Result<Self> {
        create_private_dir(&root)?;
        check_private_dir(&root)?;
        Ok(Self { root })
    }

    pub fn create(
        &self,
        definition: ProcessorDefinition,
        provenance: Option<GitProvenance>,
    ) -> Result<ProcessorVersion> {
        let composition = self.validate_definition(&definition)?;
        let _lock = UpdateLock::acquire(&self.root)?;
        self.ensure_references_active(composition.as_ref())?;
        self.create_locked(definition, provenance, None, composition)
    }

    /// Publish immutable content and advance the pointer only if the caller read
    /// the expected version. Re-publishing existing content preserves its original
    /// creation time and provenance, including when rolling the pointer back.
    pub fn publish(
        &self,
        processor_id: &str,
        definition: ProcessorDefinition,
        expected_current_version: &str,
        provenance: Option<GitProvenance>,
    ) -> Result<ProcessorVersion> {
        validate_processor_id(processor_id)?;
        validate_version(expected_current_version)?;
        let composition = self.validate_definition(&definition)?;
        let _lock = UpdateLock::acquire(&self.root)?;
        self.ensure_active(processor_id)?;
        self.ensure_references_active(composition.as_ref())?;
        let current = self.current(processor_id)?;
        if current.version != expected_current_version {
            return Err(format!(
                "Current version conflict: expected {expected_current_version}, current {}. Read the latest processor_get/list/search state and reconsider before a conditional resubmission; do not blindly retry",
                current.version
            ));
        }
        // Validate the old target before using its lineage or changing the pointer.
        if self.get(processor_id, Some(&current.version))?.lineage != current.lineage {
            return Err("Processor current-version lineage mismatch; inspect current and exact version records and reconcile the registry before continuing".into());
        }
        self.publish_locked(
            processor_id,
            definition,
            provenance,
            current.lineage,
            composition,
        )
    }

    pub fn fork(
        &self,
        source_id: &str,
        source_version: &str,
        provenance: Option<GitProvenance>,
    ) -> Result<ProcessorVersion> {
        let _lock = UpdateLock::acquire(&self.root)?;
        self.ensure_active(source_id)?;
        let source = self.get(source_id, Some(source_version))?;
        self.ensure_references_active(source.composition.as_ref())?;
        self.create_locked(
            source.definition,
            provenance,
            Some(ProcessorReference {
                processor_id: source_id.to_string(),
                version: source_version.to_string(),
            }),
            source.composition,
        )
    }

    /// Resolve current once, or read the explicitly selected immutable version.
    pub fn get(&self, processor_id: &str, version: Option<&str>) -> Result<ProcessorVersion> {
        validate_processor_id(processor_id)?;
        let current = if version.is_none() {
            self.ensure_active(processor_id)?;
            Some(self.current(processor_id)?)
        } else {
            None
        };
        let selected = version
            .map(str::to_string)
            .unwrap_or_else(|| current.as_ref().unwrap().version.clone());
        let record = self.read_version(processor_id, &selected)?;
        if current
            .as_ref()
            .is_some_and(|pointer| pointer.lineage != record.lineage)
        {
            return Err("Processor current-version lineage mismatch; inspect current and exact version records and reconcile the registry before continuing".into());
        }
        let composition = self.validate_definition(&record.definition)?;
        if record.composition != composition {
            return Err("Processor composition resolution mismatch; inspect the exact dependency versions and generated-source metadata and reconcile before installation".into());
        }
        Ok(record)
    }

    /// Check admission for direct activation. Historical exact reads and already
    /// recorded composition references intentionally remain available.
    pub fn ensure_active(&self, processor_id: &str) -> Result<()> {
        let (_, lifecycle) = self.lifecycle_snapshot(processor_id)?;
        if lifecycle.status == ProcessorStatus::Archived {
            return Err(format!("Processor {processor_id} is archived at lifecycle revision {}. Inspect processor_list/search with include_archived=true, then explicitly restore it with the latest version and revision if intended", lifecycle.lifecycle_revision));
        }
        Ok(())
    }

    pub fn archive(
        &self,
        processor_id: &str,
        expected_version: &str,
        expected_revision: u64,
    ) -> Result<ProcessorLifecycle> {
        self.transition(
            processor_id,
            expected_version,
            expected_revision,
            ProcessorStatus::Archived,
        )
    }

    pub fn restore(
        &self,
        processor_id: &str,
        expected_version: &str,
        expected_revision: u64,
    ) -> Result<ProcessorLifecycle> {
        self.transition(
            processor_id,
            expected_version,
            expected_revision,
            ProcessorStatus::Active,
        )
    }

    /// Definition and lifecycle preconditions are independent. Only a request
    /// using the current lifecycle revision may be a same-target no-op.
    fn transition(
        &self,
        processor_id: &str,
        expected_version: &str,
        expected_revision: u64,
        status: ProcessorStatus,
    ) -> Result<ProcessorLifecycle> {
        validate_processor_id(processor_id)?;
        validate_version(expected_version)?;
        let _lock = UpdateLock::acquire(&self.root)?;
        let (current, lifecycle) = self.lifecycle_snapshot(processor_id)?;
        if current.version != expected_version {
            return Err(format!("Current version conflict for {processor_id}: expected {expected_version}, current {}. Read the latest processor_get/list/search state and reconsider before a conditional resubmission; do not blindly retry", current.version));
        }
        if lifecycle.lifecycle_revision != expected_revision {
            return Err(format!("Lifecycle revision conflict for {processor_id}: expected {expected_revision}, current {} (version {}, status {:?}). Read the latest processor_list/search state with include_archived=true and reconsider before a conditional resubmission; do not blindly retry", lifecycle.lifecycle_revision, current.version, lifecycle.status));
        }
        if lifecycle.status == status {
            return Ok(lifecycle);
        }
        if self.get(processor_id, Some(&current.version))?.lineage != current.lineage {
            return Err("Processor current-version lineage mismatch; inspect the registry and reconcile its records before another transition".into());
        }
        let revision = lifecycle.lifecycle_revision.checked_add(1).ok_or(
            "Lifecycle revision overflow; inspect the registry before further transitions",
        )?;
        let next = ProcessorLifecycle {
            format_version: FORMAT_VERSION,
            processor_id: processor_id.to_string(),
            version: current.version,
            status,
            lifecycle_revision: revision,
            changed_at_unix_ms: Some(now_unix_ms()?),
        };
        let directory = self.root.join(processor_id).join("lifecycle");
        create_private_dir(&directory)?;
        check_private_dir(&directory)?;
        let revisions = directory.join("revisions");
        create_private_dir(&revisions)?;
        check_private_dir(&revisions)?;
        let event_path = revisions.join(format!("{revision:020}.json"));
        if fs::symlink_metadata(&event_path).is_ok() {
            return Err(format!("Uncommitted lifecycle event {revision} already exists for {processor_id}. Inspect the lifecycle pointer and event history and reconcile the interrupted publication before another transition"));
        }
        // The event becomes durable before the pointer commits it. Neither old
        // events nor definition files are removed during archive or restore.
        atomic_json(&event_path, &next, false)?;
        atomic_json(
            &directory.join("current.json"),
            &LifecyclePointer {
                format_version: FORMAT_VERSION,
                processor_id: processor_id.to_string(),
                lifecycle_revision: revision,
            },
            true,
        )?;
        Ok(next)
    }

    /// Keyset pagination is sorted by stable processor identity, not a snapshot.
    /// Concurrent additions before a cursor are not revisited by later pages.
    pub fn list(
        &self,
        limit: usize,
        after: Option<&str>,
        include_archived: bool,
    ) -> Result<ProcessorPage> {
        self.search("", limit, after, include_archived)
    }

    /// Literal case-insensitive substring search of identity, current version,
    /// and serialized authored definition. Metadata and older versions are not
    /// searched. An empty query lists all matching active/archive statuses.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        after: Option<&str>,
        include_archived: bool,
    ) -> Result<ProcessorPage> {
        if !(1..=100).contains(&limit) {
            return Err("Processor page limit must be between 1 and 100; choose a bounded limit and use the returned next_cursor for later pages".into());
        }
        if let Some(cursor) = after {
            validate_processor_id(cursor)?;
        }
        let query = query.to_lowercase();
        let mut identities = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let Some(identity) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if validate_processor_id(&identity).is_ok()
                && after.map_or(true, |cursor| identity.as_str() > cursor)
                && entry.file_type().map_err(io_error)?.is_dir()
            {
                identities.push(identity);
            }
        }
        identities.sort();
        let mut processors = Vec::new();
        for identity in identities {
            // Interrupted first creation can leave a directory with no published
            // current pointer. Such an identity was never admitted to discovery.
            match fs::symlink_metadata(self.root.join(&identity).join("current.json")) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error(error)),
                Ok(_) => (),
            }
            let (current, lifecycle) = self.lifecycle_snapshot(&identity)?;
            if lifecycle.status == ProcessorStatus::Archived && !include_archived {
                continue;
            }
            let record = self.get(&identity, Some(&current.version))?;
            if record.lineage != current.lineage {
                return Err("Processor current-version lineage mismatch; inspect current and exact version records and reconcile the registry before continuing".into());
            }
            let authored = serde_json::to_string(&record.definition).map_err(|e| e.to_string())?;
            if !query.is_empty()
                && !identity.to_lowercase().contains(&query)
                && !record.version.to_lowercase().contains(&query)
                && !authored.to_lowercase().contains(&query)
            {
                continue;
            }
            if processors.len() == limit {
                let next_cursor = processors
                    .last()
                    .map(|row: &ProcessorSummary| row.processor_id.clone());
                return Ok(ProcessorPage {
                    processors,
                    next_cursor,
                });
            }
            processors.push(ProcessorSummary {
                processor_id: record.processor_id,
                version: record.version,
                content_sha256: record.content_sha256,
                created_at_unix_ms: record.created_at_unix_ms,
                kind: ProcessorKind::Program,
                status: lifecycle.status,
                lifecycle_revision: lifecycle.lifecycle_revision,
                archived_at_unix_ms: if lifecycle.status == ProcessorStatus::Archived {
                    lifecycle.changed_at_unix_ms
                } else {
                    None
                },
                lineage: record.lineage,
            });
        }
        Ok(ProcessorPage {
            processors,
            next_cursor: None,
        })
    }

    fn lifecycle_pointer(&self, processor_id: &str) -> Result<Option<LifecyclePointer>> {
        let path = self.root.join(processor_id).join("lifecycle/current.json");
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("Cannot read lifecycle pointer for {processor_id}: {error}. Inspect the registry directory and permissions before continuing")),
            Ok(_) => (),
        }
        let pointer: LifecyclePointer = read_json(&path)?;
        if pointer.format_version != FORMAT_VERSION
            || pointer.processor_id != processor_id
            || pointer.lifecycle_revision == 0
        {
            return Err(format!("Invalid lifecycle pointer for {processor_id}; inspect its immutable event history and reconcile before continuing"));
        }
        Ok(Some(pointer))
    }

    /// Pair one committed lifecycle revision with one definition version. A
    /// concurrent writer can invalidate this read; report that explicitly rather
    /// than mixing a newer lifecycle event with an older current definition.
    fn lifecycle_snapshot(&self, processor_id: &str) -> Result<(Current, ProcessorLifecycle)> {
        let current = self.current(processor_id)?;
        let pointer = self.lifecycle_pointer(processor_id)?;
        let mut lifecycle = if let Some(pointer) = &pointer {
            let path = self
                .root
                .join(processor_id)
                .join("lifecycle/revisions")
                .join(format!("{:020}.json", pointer.lifecycle_revision));
            let event: ProcessorLifecycle = read_json(&path).map_err(|error| format!("Cannot read committed lifecycle revision {} for {processor_id}: {error}. Inspect the pointer and immutable event history before continuing", pointer.lifecycle_revision))?;
            if event.format_version != FORMAT_VERSION
                || event.processor_id != processor_id
                || event.lifecycle_revision != pointer.lifecycle_revision
                || event.changed_at_unix_ms.is_none()
            {
                return Err(format!("Invalid lifecycle event for {processor_id}; inspect its pointer and immutable event history before continuing"));
            }
            validate_version(&event.version)?;
            event
        } else {
            ProcessorLifecycle {
                format_version: FORMAT_VERSION,
                processor_id: processor_id.to_string(),
                version: current.version.clone(),
                status: ProcessorStatus::Active,
                lifecycle_revision: 0,
                changed_at_unix_ms: None,
            }
        };
        if self.current(processor_id)? != current
            || self.lifecycle_pointer(processor_id)? != pointer
        {
            return Err(format!("Registry state changed while reading {processor_id}. Read the latest processor_list/search state and reconsider before a conditional request"));
        }
        if lifecycle.status == ProcessorStatus::Archived && lifecycle.version != current.version {
            return Err(format!("Archived definition version mismatch for {processor_id}: lifecycle {}, current {}. Inspect the registry and reconcile before continuing", lifecycle.version, current.version));
        }
        // Event files keep the definition version present at the transition.
        // API state projects the current definition alongside the last event.
        lifecycle.version = current.version.clone();
        Ok((current, lifecycle))
    }

    fn ensure_references_active(&self, resolution: Option<&CompositionResolution>) -> Result<()> {
        if let Some(resolution) = resolution {
            // This closure came from exact resolution before the writer lock.
            // Recheck admission for every dependency under that lock so archival
            // cannot race a new publication between validation and persistence.
            for reference in resolution.dependencies.values() {
                self.ensure_active(&reference.processor_id)?;
            }
        }
        Ok(())
    }

    /// Resolve an ordinary program assembled from exact historical programs.
    /// Registry archival never rewrites an already recorded dependency closure.
    pub fn compile_composition(
        &self,
        manifest: &CompositionManifest,
    ) -> Result<CompiledComposition> {
        let nodes = self.resolve_nodes(manifest, &mut Vec::new(), &mut 0)?;
        super::composition::compile_resolved(manifest, &nodes)
    }

    fn resolve_nodes(
        &self,
        manifest: &CompositionManifest,
        stack: &mut Vec<ProcessorReference>,
        expanded: &mut usize,
    ) -> Result<BTreeMap<String, ResolvedNode>> {
        let mut nodes = BTreeMap::new();
        for (alias, reference) in &manifest.nodes {
            if stack.contains(reference) {
                return Err(format!("Cyclic exact processor reference {} at version {}. Inspect the definition-reference chain and simplify it to resolvable references", reference.processor_id, reference.version));
            }
            if stack.len() >= 128 {
                return Err("Processor reference nesting exceeds the operational traversal limit of 128; simplify the referenced program graph before resolving it".into());
            }
            if *expanded >= 4096 {
                return Err("Processor expansion exceeds the operational limit of 4096 nodes; simplify or reduce repeated program expansion before resolving it".into());
            }
            *expanded += 1;
            stack.push(reference.clone());
            let record = self.read_version(&reference.processor_id, &reference.version)?;
            let children = match &record.definition {
                ProcessorDefinition::Program(program) => {
                    if program.operation.is_some() {
                        return Err("Registered operation programs cannot participate in this ordinary rule program; use its request/response tools independently".into());
                    }
                    if record.composition.is_some() {
                        return Err("Leaf program cannot contain composition resolution metadata; inspect the exact version record and reconcile its definition kind before continuing".into());
                    }
                    validate_program(program)?;
                    BTreeMap::new()
                }
                ProcessorDefinition::Composition(definition) => {
                    let children = self.resolve_nodes(&definition.composition, stack, expanded)?;
                    let compiled =
                        super::composition::compile_resolved(&definition.composition, &children)?;
                    if record.composition.as_ref() != Some(&compiled.resolution) {
                        return Err(format!("Processor composition resolution mismatch for {} version {}; inspect the exact dependency versions and generated-source metadata and reconcile before installation", record.processor_id, record.version));
                    }
                    children
                }
            };
            stack.pop();
            nodes.insert(alias.clone(), ResolvedNode { record, children });
        }
        Ok(nodes)
    }

    fn validate_definition(
        &self,
        definition: &ProcessorDefinition,
    ) -> Result<Option<CompositionResolution>> {
        match definition {
            ProcessorDefinition::Program(program) => {
                validate_program(program)?;
                Ok(None)
            }
            ProcessorDefinition::Composition(composition) => Ok(Some(
                self.compile_composition(&composition.composition)?
                    .resolution,
            )),
        }
    }

    /// Read identity, integrity, and publication metadata without dependency
    /// traversal. Callers decide whether semantic validation may recurse.
    fn read_version(&self, processor_id: &str, selected: &str) -> Result<ProcessorVersion> {
        validate_processor_id(processor_id)?;
        validate_version(selected)?;
        let record: ProcessorVersion = read_json(&self.version_path(processor_id, selected))
            .map_err(|error| format!("Cannot read processor {processor_id} version {selected}: {error}. Use processor_list/search to discover identities and processor_get to inspect an available version"))?;
        if record.format_version != FORMAT_VERSION
            || record.processor_id != processor_id
            || record.version != selected
            || record.validation != DefinitionValidation::checked()
        {
            return Err("Invalid processor version envelope; inspect the requested identity/version and record metadata and reconcile before continuing".into());
        }
        validate_lineage(&record.lineage)?;
        let hash = definition_hash(&record.definition)?;
        if record.content_sha256 != hash || record.version != format!("sha256:{hash}") {
            return Err("Processor definition content hash mismatch; inspect the exact version file and reconcile its authored definition before continuing".into());
        }
        Ok(record)
    }

    fn current(&self, processor_id: &str) -> Result<Current> {
        validate_processor_id(processor_id)?;
        let current: Current = read_json(&self.root.join(processor_id).join("current.json"))
            .map_err(|error| format!("Cannot read current processor {processor_id}: {error}. Use processor_list/search with include_archived=true to discover valid identities"))?;
        if current.format_version != FORMAT_VERSION || current.processor_id != processor_id {
            return Err("Invalid processor current-version envelope; inspect current.json and reconcile its identity and version before continuing".into());
        }
        validate_version(&current.version)?;
        validate_lineage(&current.lineage)?;
        Ok(current)
    }

    fn create_locked(
        &self,
        definition: ProcessorDefinition,
        provenance: Option<GitProvenance>,
        lineage: Option<ProcessorReference>,
        composition: Option<CompositionResolution>,
    ) -> Result<ProcessorVersion> {
        let processor_id = format!("processor_{}", random_hex()?);
        let directory = self.root.join(&processor_id);
        // A collision fails closed; identities are never reused or overwritten.
        private_dir_builder().create(&directory).map_err(io_error)?;
        sync_directory(&self.root)?;
        private_dir_builder()
            .create(directory.join("versions"))
            .map_err(io_error)?;
        sync_directory(&directory)?;
        self.publish_locked(&processor_id, definition, provenance, lineage, composition)
    }

    fn publish_locked(
        &self,
        processor_id: &str,
        definition: ProcessorDefinition,
        provenance: Option<GitProvenance>,
        lineage: Option<ProcessorReference>,
        composition: Option<CompositionResolution>,
    ) -> Result<ProcessorVersion> {
        let content_sha256 = definition_hash(&definition)?;
        let version = format!("sha256:{content_sha256}");
        let path = self.version_path(processor_id, &version);
        let record = match fs::symlink_metadata(&path) {
            Ok(_) => {
                let record = self.get(processor_id, Some(&version))?;
                if record.lineage != lineage {
                    return Err("Processor lineage mismatch; inspect the exact version and fork source and reconcile before publication".into());
                }
                record
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let record = ProcessorVersion {
                    format_version: FORMAT_VERSION,
                    processor_id: processor_id.to_string(),
                    version: version.clone(),
                    content_sha256,
                    created_at_unix_ms: now_unix_ms()?,
                    git_provenance: provenance,
                    lineage: lineage.clone(),
                    definition,
                    validation: DefinitionValidation::checked(),
                    composition,
                };
                atomic_json(&path, &record, false)?;
                record
            }
            Err(error) => return Err(io_error(error)),
        };
        atomic_json(
            &self.root.join(processor_id).join("current.json"),
            &Current {
                format_version: FORMAT_VERSION,
                processor_id: processor_id.to_string(),
                version,
                lineage,
            },
            true,
        )?;
        Ok(record)
    }

    fn version_path(&self, processor_id: &str, version: &str) -> PathBuf {
        // Public callers validate both identifiers before path construction.
        self.root
            .join(processor_id)
            .join("versions")
            .join(format!("{}.json", &version[7..]))
    }
}

fn validate_program(definition: &ProgramDefinition) -> Result<()> {
    super::composition::validate_interface(definition)?;
    if let Some(operation) = &definition.operation {
        if !definition.operators.is_empty() {
            return Err("Typed operators cannot be combined with a registered operation; put the operator in a separate pure program".into());
        }
        super::operations::lower_registered_program(
            &operation.name,
            &operation.version,
            &definition.rules,
            definition.schemas.clone(),
        )?;
    } else {
        let schemas: BTreeMap<String, super::Schema> =
            serde_json::from_value(definition.schemas.clone())
                .map_err(|error| error.to_string())?;
        super::lower_with_operators(&definition.rules, &schemas, &definition.operators)?;
    }
    Ok(())
}

fn definition_hash(definition: &ProcessorDefinition) -> Result<String> {
    // JSON object keys are recursively ordered so map insertion order cannot
    // affect identity. Exact authored text and operation definitions do affect it.
    fn canonical(value: Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(key, value)| (key, canonical(value)))
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
            value => value,
        }
    }
    let bytes = serde_json::to_vec(&canonical(
        serde_json::to_value(definition).map_err(|error| error.to_string())?,
    ))
    .map_err(|error| error.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn validate_processor_id(id: &str) -> Result<()> {
    if !id.starts_with("processor_") || !is_hex(&id[10..], 32) {
        return Err(format!("Invalid processor identity {id:?}; expected processor_ followed by 32 lowercase hexadecimal digits. Use processor_list/search to find a valid identity"));
    }
    Ok(())
}
fn validate_version(version: &str) -> Result<()> {
    if !version.starts_with("sha256:") || !is_hex(&version[7..], 64) {
        return Err(format!("Invalid processor version {version:?}; expected sha256: followed by 64 lowercase hexadecimal digits. Use processor_get/list/search to inspect an available version"));
    }
    Ok(())
}
fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn validate_lineage(lineage: &Option<ProcessorReference>) -> Result<()> {
    if let Some(source) = lineage {
        validate_processor_id(&source.processor_id)?;
        validate_version(&source.version)?;
    }
    Ok(())
}

fn random_hex() -> Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(io_error)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn now_unix_ms() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_millis()
        .try_into()
        .map_err(|_| "Timestamp overflow".into())
}
fn io_error(error: std::io::Error) -> String {
    error.to_string()
}
fn private_dir_builder() -> fs::DirBuilder {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
}
fn create_private_dir(path: &Path) -> Result<()> {
    match private_dir_builder().create(path) {
        Ok(()) => sync_directory(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(io_error(error)),
    }
}
fn check_private_dir(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Registry must be a directory, not a symbolic link; select a dedicated private registry directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("Registry directory must be private to its owner (mode 0700); use a dedicated private directory or correct its permissions before opening it".into());
        }
    }
    #[cfg(not(unix))]
    return Err("Private local processor registries require Unix permissions; run this local registry on a supported Unix host".into());
    #[allow(unreachable_code)]
    Ok(())
}
fn private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(io_error)
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Registry records must be regular files; inspect the record path and reconcile the registry rather than following a link".into());
    }
    serde_json::from_slice(&fs::read(path).map_err(io_error)?)
        .map_err(|error| format!("Invalid registry record {}: {error}", path.display()))
}

fn atomic_json<T: Serialize>(path: &Path, value: &T, replace: bool) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    let directory = path.parent().ok_or("Missing record directory")?;
    let temporary = directory.join(format!(".write-{}", random_hex()?));
    let result = (|| {
        let mut file = private_file(&temporary).map_err(io_error)?;
        file.write_all(&bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        if replace {
            fs::rename(&temporary, path).map_err(io_error)?;
        } else {
            // Atomic no-replace publication keeps historical versions immutable.
            fs::hard_link(&temporary, path).map_err(io_error)?;
            fs::remove_file(&temporary).map_err(io_error)?;
        }
        sync_directory(directory).map_err(|error| {
            format!("Registry publication durability uncertain; inspect before retrying: {error}")
        })
    })();
    let _ = fs::remove_file(&temporary);
    result
}

struct UpdateLock {
    path: PathBuf,
}
impl UpdateLock {
    fn acquire(root: &Path) -> Result<Self> {
        check_private_dir(root)?;
        let path = root.join(".update.lock");
        let mut file = private_file(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                "Registry update lock exists; another writer may be active. A stale lock requires operator reconciliation; it is never removed automatically".into()
            } else {
                io_error(error)
            }
        })?;
        let lock = Self { path };
        writeln!(file, "pid={}", std::process::id()).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        sync_directory(root)?;
        Ok(lock)
    }
}
impl Drop for UpdateLock {
    fn drop(&mut self) {
        if fs::remove_file(&self.path).is_ok() {
            if let Some(root) = self.path.parent() {
                let _ = sync_directory(root);
            }
        }
    }
}
