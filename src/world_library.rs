//! Saved library artifacts: exact registry admission followed by catalog and
//! scenario publication. No compilation, world creation, or library-specific logic.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryArtifact {
    pub schema_version: u32,
    pub library: ArtifactLibrary,
    pub entries: Vec<ArtifactEntry>,
    pub pins: BTreeMap<String, ProcessorReference>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactLibrary {
    pub key: String,
    pub name: String,
    pub repository: String,
    pub package: String,
    pub package_version: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactEntry {
    pub key: String,
    pub name: String,
    pub description: String,
    pub source: Vec<String>,
    pub git_provenance: Option<GitProvenance>,
    pub record: ProcessorVersion,
    pub scenarios: Vec<Scenario>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryImportRequest {
    pub artifact: LibraryArtifact,
    /// Explicit source/build provenance supplied by the publisher, never
    /// inferred from package_version or a directory name.
    pub revision: String,
    #[serde(default)]
    pub dry_run: bool,
}

fn bounded(value: &str, label: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > 4096 {
        return Err(format!("{label} must contain 1–4096 bytes"));
    }
    Ok(())
}

impl WorldManager {
    /// A replayable artifact import. Validation failure writes nothing. An I/O
    /// failure may leave valid immutable records or saved scenarios; repeating
    /// the same artifact reconciles them. Existing world pins are never changed.
    pub fn library_import(&self, request: LibraryImportRequest) -> Result<Value, String> {
        let artifact = &request.artifact;
        if artifact.schema_version != 1
            || artifact.entries.is_empty()
            || artifact.entries.len() > 4096
        {
            return Err("Library artifact requires schema_version 1 and 1–4096 entries".into());
        }
        let library = &artifact.library;
        for (label, value) in [
            ("Library key", &library.key),
            ("Library name", &library.name),
            ("Repository", &library.repository),
            ("Package", &library.package),
            ("Package version", &library.package_version),
            ("Revision", &request.revision),
        ] {
            bounded(value, label)?;
        }
        let id = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&json!({
                    "schema_version":1,"key":library.key,"repository":library.repository
                }))
                .map_err(|e| e.to_string())?
            )
        );
        let mut catalog = self.stored_catalog()?;
        let mut keys = BTreeSet::new();
        let mut pins = BTreeSet::new();
        for entry in &artifact.entries {
            bounded(&entry.key, "Entry key")?;
            validate_name_description(&entry.name, &entry.description)?;
            if !keys.insert(entry.key.clone()) {
                return Err("Duplicate library entry key".into());
            }
            let record = &entry.record;
            if !pins.insert((record.processor_id.clone(), record.version.clone())) {
                return Err("A library artifact cannot name the same pin twice".into());
            }
            let pin = ProcessorReference {
                processor_id: record.processor_id.clone(),
                version: record.version.clone(),
            };
            if artifact.pins.get(&entry.key) != Some(&pin)
                || entry.git_provenance != record.git_provenance
            {
                return Err(format!(
                    "Library entry {} pin or provenance does not match its exact record",
                    entry.key
                ));
            }
            for source in &entry.source {
                bounded(source, "Source path")?;
                if std::path::Path::new(source).is_absolute()
                    || source.split('/').any(|p| p == "..")
                {
                    return Err("Library source locations must be repository-relative".into());
                }
            }
            validate_scenarios(&public_relations(record)?, &entry.scenarios)?;
            for existing in catalog["libraries"].as_array().unwrap() {
                if existing["id"] != id
                    && existing["id"] != UNASSIGNED_LIBRARY
                    && existing["processors"].as_array().unwrap().iter().any(|p| {
                        p["processor_id"] == record.processor_id && p["version"] == record.version
                    })
                {
                    return Err(format!("Pin for {} already belongs to another library; reconcile its association explicitly", entry.key));
                }
            }
        }
        if keys.len() != artifact.pins.len() {
            return Err("Library pins contain unknown keys".into());
        }
        let registry = self.registry()?;
        let records: Vec<_> = artifact.entries.iter().map(|e| e.record.clone()).collect();
        let checked = registry.import_records(records.clone(), true)?;
        if !checked.errors.is_empty() {
            return Err(format!(
                "Library admission failed: {}",
                serde_json::to_string(&checked.errors).map_err(|e| e.to_string())?
            ));
        }
        if request.dry_run {
            return Ok(
                json!({"schema_version":1,"dry_run":true,"library_id":id,"pins":artifact.pins,"records":checked.imported.len()}),
            );
        }
        let imported = registry.import_records(records, false)?;
        if !imported.errors.is_empty() {
            return Err(format!(
                "Library record publication incomplete: {}",
                serde_json::to_string(&imported.errors).map_err(|e| e.to_string())?
            ));
        }
        // Scenarios are exact-pin sidecars. Catalog publication comes last so
        // a newly visible library has all its saved scenarios in place.
        for entry in &artifact.entries {
            self.scenarios_set(
                &entry.record.processor_id,
                &entry.record.version,
                entry.scenarios.clone(),
            )?;
        }
        let libraries = catalog["libraries"].as_array_mut().unwrap();
        for existing in libraries.iter_mut() {
            existing["processors"].as_array_mut().unwrap().retain(|p| {
                !pins.contains(&(
                    p["processor_id"].as_str().unwrap_or("").into(),
                    p["version"].as_str().unwrap_or("").into(),
                ))
            });
        }
        let index = if let Some(index) = libraries.iter().position(|l| l["id"] == id) {
            index
        } else {
            libraries.push(json!({"id":id,"registered_at_unix_ms":timestamp(),"processors":[]}));
            libraries.len() - 1
        };
        let saved = &mut libraries[index];
        saved["key"] = json!(library.key);
        saved["name"] = json!(library.name);
        saved["repository"] = json!(library.repository);
        saved["revision"] = json!(request.revision);
        saved["package"] = json!(library.package);
        saved["package_version"] = json!(library.package_version);
        saved["definitions"] = json!("registered_exact_records");
        saved["provenance"] = json!("publisher_supplied");
        for entry in &artifact.entries {
            saved["processors"].as_array_mut().unwrap().push(json!({
                "key":entry.key,"processor_id":entry.record.processor_id,"version":entry.record.version,
                "name":entry.name,"description":entry.description,"source":entry.source
            }));
        }
        atomic_json(&self.build_root.join("libraries.json"), &catalog)?;
        Ok(
            json!({"schema_version":1,"dry_run":false,"library_id":id,"pins":artifact.pins,"records":imported.imported.len()}),
        )
    }
}
