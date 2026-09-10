//! Runtime-owned full checkpoints with separate object and catalog publication.
//!
//! A dedicated Iceberg table stores integrity-bound format-1 checkpoints. Call
//! `stage` to write immutable Parquet, retain its receipt, then `publish` later.
//! Native mutation acknowledgment and staging alone never imply catalog durability.
//! One cooperative publisher owns a table; no WAL or distributed fencing is claimed.
use crate::{Backend, Result};
use arrow_array::{Array, ArrayRef, BinaryArray, LargeBinaryArray, RecordBatch, StringArray};
use futures::TryStreamExt;
use iceberg::arrow::schema_to_arrow_schema;
use iceberg::spec::{
    read_data_files_from_avro, write_data_files_to_avro, DataContentType, DataFile,
    DataFileBuilder, DataFileFormat, FormatVersion, ManifestStatus, NestedField, PrimitiveType,
    Schema, Type,
};
use iceberg::transaction::{ApplyTransactionAction, Transaction};
use iceberg::writer::file_writer::{FileWriter, FileWriterBuilder, ParquetWriterBuilder};
use iceberg::{table::Table, Catalog, TableIdent};
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, io::Cursor, sync::Arc};

const MAX_BYTES: usize = 64 * 1024 * 1024;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Schema for a dedicated checkpoint table. Payload contains the runtime's exact
/// validated source, complete typed input inventory, revision and caller metadata.
/// This is a checkpoint transport, not a public query table of individual facts.
pub fn schema() -> Result<Schema> {
    Schema::builder()
        .with_schema_id(0)
        .with_fields([
            Arc::new(NestedField::required(
                1,
                "publication_id",
                Type::Primitive(PrimitiveType::String),
            )),
            Arc::new(NestedField::required(
                2,
                "checkpoint_sha256",
                Type::Primitive(PrimitiveType::String),
            )),
            Arc::new(NestedField::required(
                3,
                "checkpoint",
                Type::Primitive(PrimitiveType::Binary),
            )),
        ])
        .build()
        .map_err(err)
}
fn validate_table(table: &Table) -> Result<()> {
    if table.metadata().format_version() != FormatVersion::V3
        || table.metadata().current_schema().as_ref() != &schema()?
    {
        return Err("Requires a dedicated unmodified v3 checkpoint schema".into());
    }
    if table
        .metadata()
        .properties()
        .get("commit.retry.num-retries")
        .map(String::as_str)
        != Some("0")
    {
        return Err("Checkpoint table requires commit.retry.num-retries=0; reconcile exact receipts after conflicts".into());
    }
    if !table.metadata().default_partition_spec().is_unpartitioned() {
        return Err("Checkpoint table must be unpartitioned".into());
    }
    Ok(())
}

/// Retain this receipt before delaying publication. It contains no credentials.
/// Losing it after staging leaves an orphan object; callers must retain/retry it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedCheckpoint {
    pub publication_id: String,
    pub table_uuid: String,
    pub checkpoint_sha256: String,
    pub object_uri: String,
    pub object_sha256: String,
    pub descriptor_avro: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedCheckpoint {
    pub publication_id: String,
    pub checkpoint_sha256: String,
    pub snapshot_id: i64,
    pub adopted: bool,
}

impl Backend {
    /// Freeze acknowledged runtime state and write Parquet through the supplied
    /// table FileIO (local or object storage). Catalog visibility is unchanged.
    /// `object_uri` must be unique and immutable under the cooperative owner.
    pub async fn stage_iceberg_checkpoint(
        &self,
        table: &Table,
        publication_id: &str,
        object_uri: &str,
        metadata: Value,
    ) -> Result<StagedCheckpoint> {
        validate_table(table)?;
        if !id_valid(publication_id) {
            return Err("Invalid checkpoint publication ID".into());
        }
        let checkpoint = self.checkpoint_bytes(metadata)?;
        let digest = hash(&checkpoint);
        if table
            .file_io()
            .new_input(object_uri)
            .map_err(err)?
            .exists()
            .await
            .map_err(err)?
        {
            return Err(
                "Object already exists; retry with the retained staged receipt, never overwrite"
                    .into(),
            );
        }
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(vec![publication_id])),
            Arc::new(StringArray::from(vec![digest.as_str()])),
            Arc::new(LargeBinaryArray::from(vec![checkpoint.as_slice()])),
        ];
        let batch = RecordBatch::try_new(
            Arc::new(schema_to_arrow_schema(&schema()?).map_err(err)?),
            arrays,
        )
        .map_err(err)?;
        let mut writer = ParquetWriterBuilder::new(
            WriterProperties::builder().build(),
            table.metadata().current_schema().clone(),
        )
        .build(table.file_io().new_output(object_uri).map_err(err)?)
        .await
        .map_err(err)?;
        writer.write(&batch).await.map_err(err)?;
        let builders = writer.close().await.map_err(err)?;
        if builders.len() != 1 {
            return Err("Unexpected checkpoint file inventory".into());
        }
        let bytes = table
            .file_io()
            .new_input(object_uri)
            .map_err(err)?
            .read()
            .await
            .map_err(err)?;
        if bytes.len() > MAX_BYTES + 1024 * 1024 {
            return Err("Parquet checkpoint exceeds transport limit".into());
        }
        let mut receipt = StagedCheckpoint {
            publication_id: publication_id.into(),
            table_uuid: table.metadata().uuid().to_string(),
            checkpoint_sha256: digest,
            object_uri: object_uri.into(),
            object_sha256: hash(&bytes),
            descriptor_avro: Vec::new(),
        };
        validate_parquet(bytes.clone(), &receipt)?;
        write_data_files_to_avro(
            &mut receipt.descriptor_avro,
            vec![descriptor(table, &receipt.object_uri, bytes.len())?],
            table.metadata().default_partition_type(),
            FormatVersion::V3,
        )
        .map_err(err)?;
        Ok(receipt)
    }

    /// Read the explicitly selected publication through a pinned Iceberg scan,
    /// then use runtime-owned checkpoint validation and candidate activation.
    pub async fn restore_iceberg_checkpoint(
        &mut self,
        catalog: &dyn Catalog,
        ident: &TableIdent,
        receipt: &StagedCheckpoint,
    ) -> Result<Value> {
        let table = catalog.load_table(ident).await.map_err(err)?;
        let snapshot = find_snapshot(&table, receipt)?.ok_or("Checkpoint not catalog-visible")?;
        let bytes = read_checkpoint(&table, snapshot, receipt).await?;
        self.restore_checkpoint_bytes(&bytes)
    }
}

fn find_snapshot(table: &Table, receipt: &StagedCheckpoint) -> Result<Option<i64>> {
    validate_table(table)?;
    if !id_valid(&receipt.publication_id)
        || table.metadata().uuid().to_string() != receipt.table_uuid
    {
        return Err("Checkpoint table/identity mismatch".into());
    }
    let mut found = None;
    for snapshot in table.metadata().snapshots() {
        let p = &snapshot.summary().additional_properties;
        if p.get("ddlog.publication-id") == Some(&receipt.publication_id) {
            if p.get("ddlog.checkpoint-sha256") != Some(&receipt.checkpoint_sha256)
                || p.get("ddlog.object-sha256") != Some(&receipt.object_sha256)
                || p.get("ddlog.object-uri") != Some(&receipt.object_uri)
            {
                return Err("Publication ID reused with different checkpoint".into());
            }
            if found.replace(snapshot.snapshot_id()).is_some() {
                return Err("Ambiguous duplicate publication snapshots".into());
            }
        }
    }
    Ok(found)
}
fn descriptor(table: &Table, uri: &str, size: usize) -> Result<DataFile> {
    descriptor_at(table, uri, size, None)
}
fn descriptor_at(
    table: &Table,
    uri: &str,
    size: usize,
    first_row_id: Option<i64>,
) -> Result<DataFile> {
    DataFileBuilder::default()
        .content(DataContentType::Data)
        .file_path(uri.to_string())
        .file_format(DataFileFormat::Parquet)
        .record_count(1)
        .first_row_id(first_row_id)
        .file_size_in_bytes(size as u64)
        .partition_spec_id(table.metadata().default_partition_spec_id())
        .build()
        .map_err(err)
}
fn payload_at(array: &dyn Array, row: usize) -> Result<&[u8]> {
    if array.is_null(row) {
        return Err("Null checkpoint payload".into());
    }
    if let Some(values) = array.as_any().downcast_ref::<LargeBinaryArray>() {
        return Ok(values.value(row));
    }
    if let Some(values) = array.as_any().downcast_ref::<BinaryArray>() {
        return Ok(values.value(row));
    }
    Err("Invalid checkpoint payload column".into())
}
async fn checked_object(table: &Table, receipt: &StagedCheckpoint) -> Result<bytes::Bytes> {
    let input = table
        .file_io()
        .new_input(&receipt.object_uri)
        .map_err(err)?;
    if input.metadata().await.map_err(err)?.size > (MAX_BYTES + 1024 * 1024) as u64 {
        return Err("Checkpoint object exceeds limit".into());
    }
    let bytes = input.read().await.map_err(err)?;
    if bytes.len() > MAX_BYTES + 1024 * 1024 || hash(&bytes) != receipt.object_sha256 {
        return Err("Immutable checkpoint object changed".into());
    }
    Ok(bytes)
}
fn validate_parquet(bytes: bytes::Bytes, receipt: &StagedCheckpoint) -> Result<()> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes).map_err(err)?;
    if builder.metadata().file_metadata().num_rows() != 1 {
        return Err("Checkpoint object must contain one row".into());
    }
    let columns = builder.parquet_schema().columns();
    let names = ["publication_id", "checkpoint_sha256", "checkpoint"];
    if columns.len() != names.len()
        || columns.iter().enumerate().any(|(index, column)| {
            let info = column.self_type().get_basic_info();
            column.name() != names[index] || !info.has_id() || info.id() != (index + 1) as i32
        })
    {
        return Err("Checkpoint Parquet field inventory/IDs do not match table schema".into());
    }
    if builder.metadata().row_groups().iter().any(|group| {
        group.total_byte_size() < 0
            || group.total_byte_size() as u64 > (MAX_BYTES + 1024 * 1024) as u64
    }) {
        return Err("Checkpoint decoded page size exceeds limit".into());
    }
    let reader = builder.build().map_err(err)?;
    let mut count = 0;
    for batch in reader {
        let batch = batch.map_err(err)?;
        for row in 0..batch.num_rows() {
            let ids = batch
                .column_by_name("publication_id")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or("Invalid checkpoint ID column")?;
            let hashes = batch
                .column_by_name("checkpoint_sha256")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or("Invalid digest column")?;
            let payloads = batch
                .column_by_name("checkpoint")
                .ok_or("Missing payload")?;
            if ids.is_null(row)
                || hashes.is_null(row)
                || payloads.is_null(row)
                || ids.value(row) != receipt.publication_id
                || hashes.value(row) != receipt.checkpoint_sha256
                || hash(payload_at(payloads.as_ref(), row)?) != receipt.checkpoint_sha256
            {
                return Err("Checkpoint payload/receipt mismatch".into());
            }
            crate::checkpoint::validate_encoded(payload_at(payloads.as_ref(), row)?)?;
            count += 1;
        }
    }
    if count != 1 {
        return Err("Missing checkpoint row".into());
    }
    Ok(())
}
async fn read_checkpoint(
    table: &Table,
    snapshot: i64,
    receipt: &StagedCheckpoint,
) -> Result<Vec<u8>> {
    validate_parquet(checked_object(table, receipt).await?, receipt)?;
    let selected = table
        .metadata()
        .snapshot_by_id(snapshot)
        .ok_or("Snapshot missing")?;
    let manifests = table
        .manifest_list_reader(selected)
        .load()
        .await
        .map_err(err)?;
    let mut added = Vec::new();
    for manifest in manifests.entries() {
        for entry in table
            .manifest_reader()
            .read(manifest)
            .await
            .map_err(err)?
            .entries()
        {
            if entry.status == ManifestStatus::Added && entry.snapshot_id() == Some(snapshot) {
                added.push(entry.data_file.clone());
            }
        }
    }
    let object_size = table
        .file_io()
        .new_input(&receipt.object_uri)
        .map_err(err)?
        .metadata()
        .await
        .map_err(err)?
        .size;
    let first_row_id = selected
        .first_row_id()
        .map(i64::try_from)
        .transpose()
        .map_err(err)?;
    if first_row_id.is_none()
        || added
            != vec![descriptor_at(
                table,
                &receipt.object_uri,
                object_size as usize,
                first_row_id,
            )?]
    {
        return Err(
            "Publication snapshot has wrong exact added file set or v3 row allocation".into(),
        );
    }
    let scan = table.scan().snapshot_id(snapshot).build().map_err(err)?;
    let mut batches = scan.to_arrow().await.map_err(err)?;
    let mut found = None;
    let mut scanned = 0usize;
    while let Some(batch) = batches.try_next().await.map_err(err)? {
        let ids = batch
            .column_by_name("publication_id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .ok_or("Invalid checkpoint ID column")?;
        let hashes = batch
            .column_by_name("checkpoint_sha256")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .ok_or("Invalid checkpoint digest column")?;
        let payloads = batch
            .column_by_name("checkpoint")
            .ok_or("Missing payload")?;
        for row in 0..batch.num_rows() {
            scanned += payload_at(payloads.as_ref(), row)?.len();
            if scanned > 2 * MAX_BYTES {
                return Err("Checkpoint snapshot scan exceeds 128 MiB; explicit retention/compaction required".into());
            }
            if ids.is_null(row) || hashes.is_null(row) || payloads.is_null(row) {
                return Err("Null checkpoint row".into());
            }
            if ids.value(row) == receipt.publication_id {
                let bytes = payload_at(payloads.as_ref(), row)?;
                if bytes.len() > MAX_BYTES
                    || hashes.value(row) != receipt.checkpoint_sha256
                    || hash(bytes) != receipt.checkpoint_sha256
                    || found.is_some()
                {
                    return Err("Invalid/duplicate checkpoint payload".into());
                }
                found = Some(bytes.to_vec());
            }
        }
    }
    found.ok_or_else(|| "Publication snapshot does not contain its checkpoint".into())
}

/// Publish previously written bytes. Fresh catalog readback is authoritative even
/// after an uncertain commit response. Retry the exact retained receipt. The
/// table requires one cooperative publisher and retained snapshot history.
pub async fn publish(
    catalog: &dyn Catalog,
    ident: &TableIdent,
    receipt: &StagedCheckpoint,
) -> Result<PublishedCheckpoint> {
    let table = catalog.load_table(ident).await.map_err(err)?;
    if let Some(snapshot_id) = find_snapshot(&table, receipt)? {
        read_checkpoint(&table, snapshot_id, receipt).await?;
        return Ok(PublishedCheckpoint {
            publication_id: receipt.publication_id.clone(),
            checkpoint_sha256: receipt.checkpoint_sha256.clone(),
            snapshot_id,
            adopted: true,
        });
    }
    if receipt.descriptor_avro.len() > 1024 * 1024 {
        return Err("Oversized checkpoint descriptor".into());
    }
    let files = read_data_files_from_avro(
        &mut Cursor::new(&receipt.descriptor_avro),
        table.metadata().current_schema(),
        table.metadata().default_partition_spec_id(),
        table.metadata().default_partition_type(),
        FormatVersion::V3,
    )
    .map_err(err)?;
    if files.len() != 1
        || files[0].file_path() != receipt.object_uri
        || files[0].record_count() != 1
    {
        return Err("Invalid checkpoint descriptor".into());
    }
    let object = checked_object(&table, receipt).await?;
    if files != vec![descriptor(&table, &receipt.object_uri, object.len())?] {
        return Err(
            "Checkpoint descriptor differs from verified object; metrics are not accepted".into(),
        );
    }
    validate_parquet(object, receipt)?;
    let tx = Transaction::new(&table);
    let outcome = tx
        .fast_append()
        .with_check_duplicate(true)
        .add_data_files(files)
        .set_snapshot_properties(HashMap::from([
            (
                "ddlog.publication-id".into(),
                receipt.publication_id.clone(),
            ),
            (
                "ddlog.checkpoint-sha256".into(),
                receipt.checkpoint_sha256.clone(),
            ),
            ("ddlog.object-sha256".into(), receipt.object_sha256.clone()),
            ("ddlog.object-uri".into(), receipt.object_uri.clone()),
        ]))
        .apply(tx)
        .map_err(err)?
        .commit(catalog)
        .await;
    let fresh = catalog.load_table(ident).await.map_err(err)?;
    if let Some(snapshot_id) = find_snapshot(&fresh, receipt)? {
        read_checkpoint(&fresh, snapshot_id, receipt).await?;
        return Ok(PublishedCheckpoint {
            publication_id: receipt.publication_id.clone(),
            checkpoint_sha256: receipt.checkpoint_sha256.clone(),
            snapshot_id,
            adopted: false,
        });
    }
    Err(format!(
        "Checkpoint publication unconfirmed; retry retained receipt ({})",
        if outcome.is_err() {
            "catalog error"
        } else {
            "readback missing"
        }
    ))
}
