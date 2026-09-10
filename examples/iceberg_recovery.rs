//! Fresh-process acceptance for native DDlog -> Parquet -> Iceberg -> native restore.
#[path = "support/r2_rest.rs"]
mod r2_rest;
use ddlog_runtime::{
    iceberg_checkpoint::{self, StagedCheckpoint},
    Backend,
};
use iceberg::{
    io::{LocalFsStorageFactory, StorageFactory},
    Catalog, CatalogBuilder, NamespaceIdent, TableCreation, TableIdent,
};
use iceberg_catalog_sql::{SqlBindStyle, SqlCatalogBuilder};
use serde_json::{json, Value};
use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn write(path: PathBuf, value: &Value) -> Result<(), String> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(error)?;
    f.write_all(
        serde_json::to_string_pretty(value)
            .map_err(error)?
            .as_bytes(),
    )
    .map_err(error)?;
    f.sync_all().map_err(error)
}
#[tokio::main]
async fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 5 {
        return Err("Usage: iceberg_recovery stage|publish|restore ABSOLUTE_RUN_DIR ABSOLUTE_NATIVE_DRIVER WAREHOUSE_URI".into());
    }
    let mode = &args[1];
    let root = PathBuf::from(&args[2]);
    let driver = PathBuf::from(&args[3]);
    let warehouse = &args[4];
    if !root.is_absolute() || !driver.is_absolute() {
        return Err("Absolute paths required".into());
    }
    let factory: Arc<dyn StorageFactory> = if warehouse.starts_with("r2://") {
        let prefix = warehouse
            .strip_prefix("r2://archetype-staging/")
            .ok_or("Only authorized staging bucket accepted")?;
        Arc::new(r2_rest::R2Rest {
            account: std::env::var("CLOUDFLARE_ACCOUNT_ID").map_err(error)?,
            bucket: "archetype-staging".into(),
            prefix: prefix.into(),
        })
    } else {
        Arc::new(LocalFsStorageFactory)
    };
    if mode == "stage" {
        fs::create_dir(&root).map_err(error)?;
    }
    let catalog = SqlCatalogBuilder::default()
        .uri(format!(
            "sqlite://{}?mode=rwc",
            root.join("catalog.sqlite").display()
        ))
        .warehouse_location(warehouse.clone())
        .sql_bind_style(SqlBindStyle::QMark)
        .prop("pool.max-connections", "1")
        .with_storage_factory(factory)
        .load("runtime-checkpoints", HashMap::new())
        .await
        .map_err(error)?;
    let ident = TableIdent::from_strs(["runtime", "checkpoints"]).map_err(error)?;
    if mode == "stage" {
        catalog
            .create_namespace(&NamespaceIdent::new("runtime".into()), HashMap::new())
            .await
            .map_err(error)?;
        let table = catalog
            .create_table(
                &NamespaceIdent::new("runtime".into()),
                TableCreation::builder()
                    .name("checkpoints".into())
                    .location(format!("{warehouse}/table"))
                    .schema(iceberg_checkpoint::schema()?)
                    .format_version(iceberg::spec::FormatVersion::V3)
                    .properties(HashMap::from([(
                        "commit.retry.num-retries".into(),
                        "0".into(),
                    )]))
                    .build(),
            )
            .await
            .map_err(error)?;
        let mut live = Backend::new(root.join("live"), driver);
        live.install("reach(X,Y) :- edge(X,Y). reach(X,Z) :- reach(X,Y), edge(Y,Z).",json!({"edge":{"input":true,"fields":["int","int"]},"empty":{"input":true,"fields":["string"]},"reach":{"input":false,"fields":["int","int"]}}))?;
        live.apply_without_deltas(&json!([{"op":"insert","predicate":"edge","values":[1,2]},{"op":"insert","predicate":"edge","values":[2,3]}]))?;
        let expected = live.query_typed("reach")?;
        let revision = live.revision();
        let receipt = live
            .stage_iceberg_checkpoint(
                &table,
                "boundary-2",
                &format!("{warehouse}/external/checkpoint.parquet"),
                json!({"purpose":"native-iceberg-acceptance"}),
            )
            .await?;
        let fresh = catalog.load_table(&ident).await.map_err(error)?;
        if fresh.metadata().current_snapshot_id().is_some() {
            return Err("Staging unexpectedly published a snapshot".into());
        }
        live.apply_without_deltas(&json!([{"op":"insert","predicate":"edge","values":[3,4]}]))?;
        if live.query_typed("reach")? == expected {
            return Err("Unpublished mutation did not change outputs".into());
        }
        write(
            root.join("staged.json"),
            &serde_json::to_value(&receipt).map_err(error)?,
        )?;
        write(
            root.join("expected.json"),
            &json!({"rows":expected,"revision":revision,"inputs":2,"empty_relation_retained":true}),
        )?;
        println!(
            "{}",
            json!({"stage":"passed","object_written":true,"catalog_visible":false,"unpublished_mutation":true})
        );
    } else {
        let receipt: StagedCheckpoint =
            serde_json::from_slice(&fs::read(root.join("staged.json")).map_err(error)?)
                .map_err(error)?;
        if mode == "publish" {
            let first = iceberg_checkpoint::publish(&catalog, &ident, &receipt).await?;
            let retry = iceberg_checkpoint::publish(&catalog, &ident, &receipt).await?;
            if !retry.adopted || retry.snapshot_id != first.snapshot_id {
                return Err("Retry duplicated publication".into());
            }
            let mut conflict = receipt.clone();
            conflict.checkpoint_sha256 = "wrong".into();
            if iceberg_checkpoint::publish(&catalog, &ident, &conflict)
                .await
                .is_ok()
            {
                return Err("Conflicting identity accepted".into());
            }
            write(
                root.join("publication.json"),
                &json!({"first":first,"retry":retry,"collision_rejected":true}),
            )?;
            println!(
                "{}",
                json!({"publish":"passed","retry_adopted":true,"collision_rejected":true})
            );
        } else if mode == "restore" {
            let expected: Value =
                serde_json::from_slice(&fs::read(root.join("expected.json")).map_err(error)?)
                    .map_err(error)?;
            let mut restored = Backend::new(root.join("restored"), driver);
            let metadata = restored
                .restore_iceberg_checkpoint(&catalog, &ident, &receipt)
                .await?;
            let mut rows = restored.query_typed("reach")?;
            rows.sort_by_key(|r| serde_json::to_string(r).expect("JSON row"));
            let mut wanted: Vec<Vec<Value>> =
                serde_json::from_value(expected["rows"].clone()).map_err(error)?;
            wanted.sort_by_key(|r| serde_json::to_string(r).expect("JSON row"));
            if rows != wanted
                || restored.revision() != expected["revision"].as_u64().ok_or("revision")?
                || !restored.export_inputs()?.contains_key("empty")
            {
                return Err("Restored state mismatch".into());
            }
            restored.apply_without_deltas(
                &json!([{"op":"delete","predicate":"edge","values":[2,3]}]),
            )?;
            if restored.query_typed("reach")? != vec![vec![json!(1), json!(2)]] {
                return Err("Post-restore mutation failed".into());
            }
            write(
                root.join("restored.json"),
                &json!({"passed":true,"rows":rows,"metadata":metadata,"revision_restored":true,"empty_relation_retained":true,"post_restore_retraction":true,"remote":warehouse.starts_with("r2://")}),
            )?;
            println!(
                "{}",
                json!({"restore":"passed","fresh_process":true,"post_restore_retraction":true})
            );
        } else {
            return Err("Unknown phase".into());
        }
    }
    Ok(())
}
