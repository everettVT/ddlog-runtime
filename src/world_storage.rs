//! One trusted local Iceberg profile, off-lock storage jobs, and exact receipts.
//! Catalog visibility is independent of the JSON durable-admission policy.
use super::{persistence, World, WorldManager};
#[cfg(feature = "iceberg")]
use persistence::Receipt;
use persistence::{Publication, RestoreSource};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
#[cfg(feature = "iceberg")]
use std::path::PathBuf;
type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Location {
    pub profile: String,
    pub profile_sha256: String,
    pub table: Vec<String>,
    pub table_uuid: Option<String>,
    pub object_uri: String,
    pub object_sha256: Option<String>,
    pub envelope_sha256: Option<String>,
    /// Decimal text preserves all 64 bits through browser JSON round trips.
    pub snapshot_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointStage {
    pub id: String,
    pub expected_generation: u64,
    pub expected_revision: u64,
    pub profile: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointPublish {
    pub id: String,
    pub receipt: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointQuery {
    pub id: String,
    pub receipt_id: String,
}
fn entry(p: &Publication, dir: &Path) -> Value {
    let present = std::fs::symlink_metadata(dir.join("checkpoint.json"))
        .is_ok_and(|m| m.is_file() && m.len() <= crate::checkpoint::MAX_BYTES);
    json!({"schema_version":1,"receipt":p.receipt,"status":p.status,"error":p.error,
        "availability":if p.receipt.format=="iceberg" {"catalog_unverified"} else if present {"present_unverified"} else {"missing_or_invalid"}})
}
fn observe_interrupted(entry: &mut Value) {
    if entry["receipt"]["format"] == "iceberg"
        && matches!(entry["status"].as_str(), Some("staging" | "publishing"))
    {
        entry["status"] = json!("uncertain");
    }
}
impl WorldManager {
    pub fn checkpoint_status(&mut self, request: CheckpointQuery) -> Result<Value> {
        self.poll_storage();
        if !self.worlds.contains_key(&request.id) {
            return Err("Unknown world".into());
        }
        persistence::component(&request.receipt_id)?;
        let root = persistence::checkpoint_root(&self.build_root, &request.id)?;
        persistence::existing_dir(&root)?;
        let dir = root.join(&request.receipt_id);
        persistence::existing_dir(&dir)?;
        let p = persistence::read_publication(&dir)?;
        if p.receipt.origin.world_id != request.id || p.receipt.receipt_id != request.receipt_id {
            return Err("Receipt location mismatch".into());
        }
        let mut result = entry(&p, &dir);
        self.storage_configuration.observe_entry(&mut result);
        Ok(result)
    }
    pub(super) fn poll_storage(&mut self) {
        self.storage_configuration.poll();
    }
}
#[cfg(not(feature = "iceberg"))]
#[derive(Default)]
pub(super) struct Configuration {
    _private: (),
}
#[cfg(not(feature = "iceberg"))]
impl Configuration {
    pub fn observe_entry(&self, entry: &mut Value) {
        observe_interrupted(entry);
    }
    pub fn cancel(&mut self, _: &str) {}
    pub fn poll(&mut self) {}
    pub fn status(&self, _: &World, _: &Value) -> Value {
        json!({"schema_version":1,"status":"unavailable","reason":"Build with the iceberg feature",
            "profile":null,"publication_ack":"catalog_visibility","admission_durability":"json",
            "published_revision":null,"error":null,"job":null})
    }
}
#[cfg(not(feature = "iceberg"))]
impl WorldManager {
    pub fn configure_storage(&mut self, _: &Path) -> Result<()> {
        Err("Build with the iceberg feature".into())
    }
    pub fn checkpoint_stage(&mut self, _: CheckpointStage) -> Result<Value> {
        Err("Build with the iceberg feature".into())
    }
    pub fn checkpoint_publish(&mut self, _: CheckpointPublish) -> Result<Value> {
        Err("Build with the iceberg feature".into())
    }
    pub(super) fn iceberg_restore_source(&self, _: &Publication) -> Result<RestoreSource> {
        Err("Build with the iceberg feature".into())
    }
}
#[cfg(feature = "iceberg")]
pub(super) use enabled::{Configuration, Read};
#[cfg(feature = "iceberg")]
mod enabled {
    use super::*;
    use crate::iceberg_checkpoint::{self, StagedCheckpoint};
    use crate::processes::ProcessControl;
    use iceberg::{Catalog, CatalogBuilder, NamespaceIdent, TableCreation, TableIdent};
    use iceberg_catalog_sql::{SqlBindStyle, SqlCatalogBuilder};
    use std::collections::HashMap;
    use std::fs::{self, File};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    };
    use std::time::Duration;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FileConfig {
        schema_version: u32,
        profile: ProfileConfig,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ProfileConfig {
        name: String,
        root: PathBuf,
        #[serde(default = "timeout")]
        timeout_ms: u64,
    }
    fn timeout() -> u64 {
        30_000
    }
    struct Profile {
        name: String,
        root: PathBuf,
        sha256: String,
        timeout_ms: u64,
        _lock: File,
    }
    impl Profile {
        fn uri(&self, path: &str) -> String {
            // This pinned LocalFs FileIO strips file:// literally; percent-encoding
            // would select a different directory. SQLite below does use URI syntax.
            format!(
                "file://{}",
                self.root.join("warehouse").join(path).display()
            )
        }
        fn location(&self, id: &str) -> Location {
            Location {
                profile: self.name.clone(),
                profile_sha256: self.sha256.clone(),
                table: vec!["runtime".into(), "checkpoints".into()],
                table_uuid: None,
                object_uri: self.uri(&format!("objects/{id}.parquet")),
                object_sha256: None,
                envelope_sha256: None,
                snapshot_id: None,
            }
        }
        fn check(&self, receipt: &Receipt) -> Result<()> {
            let location = receipt
                .storage
                .as_ref()
                .ok_or("Missing Iceberg storage identity")?;
            let expected = self.location(&receipt.receipt_id);
            if receipt.format != "iceberg"
                || location.profile != expected.profile
                || location.profile_sha256 != expected.profile_sha256
                || location.table != expected.table
                || location.object_uri != expected.object_uri
            {
                return Err(
                    "Iceberg receipt does not match the configured local profile/location".into(),
                );
            }
            Ok(())
        }
    }
    fn encoded_path(path: &Path) -> String {
        path.to_string_lossy()
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect()
    }
    fn ident() -> TableIdent {
        TableIdent::from_strs(["runtime", "checkpoints"]).expect("constant table identity")
    }
    async fn catalog(profile: &Profile, create: bool) -> Result<Arc<dyn Catalog>> {
        let db = profile.root.join("catalog.sqlite");
        if let Ok(meta) = fs::symlink_metadata(&db) {
            if !meta.is_file() {
                return Err("Invalid local catalog file".into());
            }
        }
        if !create && !db.is_file() {
            return Err("Local Iceberg catalog is missing".into());
        }
        let catalog = SqlCatalogBuilder::default()
            .uri(format!(
                "sqlite://{}?mode={}",
                encoded_path(&db),
                if create { "rwc" } else { "rw" }
            ))
            .warehouse_location(profile.uri(""))
            .sql_bind_style(SqlBindStyle::QMark)
            .prop("pool.max-connections", "1")
            .with_storage_factory(Arc::new(iceberg::io::LocalFsStorageFactory))
            .load("managed-checkpoints", HashMap::new())
            .await
            .map_err(|e| e.to_string())?;
        if create
            && !catalog
                .table_exists(&ident())
                .await
                .map_err(|e| e.to_string())?
        {
            let namespace = NamespaceIdent::new("runtime".into());
            if !catalog
                .namespace_exists(&namespace)
                .await
                .map_err(|e| e.to_string())?
            {
                catalog
                    .create_namespace(&namespace, HashMap::new())
                    .await
                    .map_err(|e| e.to_string())?;
            }
            catalog
                .create_table(
                    &namespace,
                    TableCreation::builder()
                        .name("checkpoints".into())
                        .location(profile.uri("table"))
                        .schema(iceberg_checkpoint::schema()?)
                        .format_version(iceberg::spec::FormatVersion::V3)
                        .properties(HashMap::from([(
                            "commit.retry.num-retries".into(),
                            "0".into(),
                        )]))
                        .build(),
                )
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(Arc::new(catalog))
    }
    fn run<T>(
        timeout_ms: u64,
        cancelled: impl Fn() -> bool,
        future: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let result=runtime.block_on(async {
            tokio::select! {
                result=tokio::time::timeout(Duration::from_millis(timeout_ms),future)=>result.map_err(|_|"Storage deadline elapsed; publication may have occurred".to_string())?,
                _=async {loop {if cancelled(){break;}tokio::time::sleep(Duration::from_millis(10)).await;}}=>Err("Storage operation cancelled; publication may have occurred".into()),
            }
        });
        // Cancellation does not roll back already dispatched filesystem/SQLite IO.
        // Do not wait indefinitely for blocking IO during owner shutdown.
        runtime.shutdown_timeout(Duration::from_millis(100));
        result
    }
    struct Job {
        world: String,
        generation: u64,
        receipt_id: String,
        phase: String,
        manifest: PathBuf,
        cancel: Arc<AtomicBool>,
        receiver: mpsc::Receiver<Result<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Job {
        fn drop(&mut self) {
            self.cancel.store(true, Ordering::SeqCst);
        }
    }
    #[derive(Default)]
    pub(crate) struct Configuration {
        profile: Option<Arc<Profile>>,
        job: Option<Job>,
        error: Option<String>,
        uncertain: Option<(String, String)>,
    }
    impl Configuration {
        pub fn observe_entry(&self, entry: &mut Value) {
            // An interrupted job or failed manifest write cannot promise progress.
            // The active job, if any, overrides this below until completion.
            observe_interrupted(entry);
            if self.uncertain.as_ref().is_some_and(|(world, receipt)| {
                entry["receipt"]["origin"]["world_id"] == *world
                    && entry["receipt"]["receipt_id"] == *receipt
            }) {
                entry["status"] = json!("uncertain");
                entry["error"] = json!(self.error);
            }
            if let Some(job) = &self.job {
                if entry["receipt"]["origin"]["world_id"] == job.world
                    && entry["receipt"]["receipt_id"] == job.receipt_id
                {
                    // A renamed manifest is visible before its final fsync/send.
                    // Do not acknowledge completion until the job itself finished.
                    entry["status"] = json!(job.phase);
                }
            }
        }
        pub fn cancel(&mut self, id: &str) {
            if let Some(job) = &self.job {
                if job.world == id {
                    job.cancel.store(true, Ordering::SeqCst);
                }
            }
        }
        pub fn poll(&mut self) {
            let result = self.job.as_ref().map(|j| j.receiver.try_recv());
            let result = match result {
                Some(Ok(r)) => Some(r),
                Some(Err(mpsc::TryRecvError::Disconnected)) => Some(Err(
                    "Storage worker exited without an outcome; inspect retained receipt".into(),
                )),
                _ => None,
            };
            if let Some(result) = result {
                if let Some(mut job) = self.job.take() {
                    if let Some(thread) = job.thread.take() {
                        let _ = thread.join();
                    }
                    if let Err(error) = &result {
                        self.uncertain = Some((job.world.clone(), job.receipt_id.clone()));
                        if let Some(dir) = job.manifest.parent() {
                            if let Ok(mut p) = persistence::read_publication(dir) {
                                p.status = "uncertain".into();
                                p.error = Some(error.clone());
                                let _ = persistence::write_record(&job.manifest, &p);
                            }
                        }
                    }
                }
                self.error = result.err();
            }
        }
        pub fn status(&self, world: &World, persistence: &Value) -> Value {
            let published = persistence["checkpoints"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| {
                    p["receipt"]["format"] == "iceberg"
                        && p["status"] == "published"
                        && p["receipt"]["origin"]["generation"] == world.generation
                })
                .filter_map(|p| p["receipt"]["origin"]["revision"].as_u64())
                .chain(
                    world
                        .persistence
                        .restored_from
                        .iter()
                        .filter(|r| r.format == "iceberg")
                        .map(|r| r.origin.revision),
                )
                .max();
            json!({"schema_version":1,"status":if self.profile.is_some(){"configured"}else{"not_configured"},"profile":self.profile.as_ref().map(|p|&p.name),
                "publication_ack":"catalog_visibility","admission_durability":"json","published_revision":published,"error":self.error,
                "job":self.job.as_ref().map(|j|json!({"id":j.world,"generation":j.generation,"receipt_id":j.receipt_id,"cancel_requested":j.cancel.load(Ordering::SeqCst)}))})
        }
        fn configured(&self) -> Result<Arc<Profile>> {
            self.profile
                .clone()
                .ok_or("No local Iceberg profile configured".into())
        }
    }
    pub(crate) struct Read {
        profile: Arc<Profile>,
        receipt: StagedCheckpoint,
        snapshot: i64,
    }
    impl Read {
        pub fn load(self, control: ProcessControl) -> Result<Vec<u8>> {
            run(self.profile.timeout_ms, || control.stopped(), async {
                let catalog = catalog(&self.profile, false).await?;
                let (snapshot, bytes) =
                    iceberg_checkpoint::load_publication(catalog.as_ref(), &ident(), &self.receipt)
                        .await?;
                if snapshot != self.snapshot {
                    return Err("Published snapshot identity mismatch".into());
                }
                Ok(bytes)
            })
        }
    }
    fn staged(p: &Publication, profile: &Profile) -> Result<StagedCheckpoint> {
        profile.check(&p.receipt)?;
        let staged: StagedCheckpoint = serde_json::from_value(
            p.staged
                .clone()
                .ok_or("No retained staged receipt; staging cannot be retried implicitly")?,
        )
        .map_err(|e| e.to_string())?;
        let location = p.receipt.storage.as_ref().unwrap();
        if staged.publication_id != p.receipt.receipt_id
            || Some(&staged.table_uuid) != location.table_uuid.as_ref()
            || staged.object_uri != location.object_uri
            || Some(&staged.object_sha256) != location.object_sha256.as_ref()
            || Some(&staged.checkpoint_sha256) != location.envelope_sha256.as_ref()
        {
            return Err("Managed and staged storage identities differ".into());
        }
        Ok(staged)
    }
    fn stage_form(mut r: Receipt) -> Receipt {
        r.published_at_unix_ms = None;
        if let Some(s) = &mut r.storage {
            s.snapshot_id = None;
        }
        r
    }
    enum Work {
        Stage(Vec<u8>),
        Publish(StagedCheckpoint),
    }
    impl WorldManager {
        pub fn configure_storage(&mut self, path: &Path) -> Result<()> {
            if self.storage_configuration.profile.is_some()
                || self
                    .worlds
                    .values()
                    .any(|w| w.instance.is_some() || w.pending.is_some())
            {
                return Err("Storage profile is startup-only".into());
            }
            if !path.is_absolute() {
                return Err("Storage profile file must be absolute".into());
            }
            use std::os::unix::fs::MetadataExt;
            let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
            if !meta.is_file()
                || meta.uid() != unsafe { libc::geteuid() }
                || meta.mode() & 0o022 != 0
            {
                return Err(
                    "Storage profile must be a same-user regular file, not group/world writable"
                        .into(),
                );
            }
            let config: FileConfig =
                serde_json::from_slice(&persistence::read_bounded(path, 1024 * 1024)?)
                    .map_err(|e| e.to_string())?;
            if config.schema_version != 1
                || !config.profile.root.is_absolute()
                || !(1..=120_000).contains(&config.profile.timeout_ms)
            {
                return Err("Invalid local storage profile version, root or timeout".into());
            }
            super::super::workers::token(&config.profile.name)?;
            persistence::private_dir(&config.profile.root)?;
            let root = fs::canonicalize(config.profile.root).map_err(|e| e.to_string())?;
            let lock = super::super::lock_owner(&root)?;
            persistence::private_dir(&root.join("warehouse"))?;
            persistence::private_dir(&root.join("warehouse/objects"))?;
            let sha256 = super::super::workers::digest(
                &json!({"name":config.profile.name,"root":root,"kind":"local_sqlite"}),
            )?;
            self.storage_configuration.profile = Some(Arc::new(Profile {
                name: config.profile.name,
                root,
                sha256,
                timeout_ms: config.profile.timeout_ms,
                _lock: lock,
            }));
            // An interrupted intent is never auto-published or adopted on restart.
            for id in self.worlds.keys() {
                let root = persistence::checkpoint_root(&self.build_root, id)?;
                if !root.exists() {
                    continue;
                }
                persistence::existing_dir(&root)?;
                for item in fs::read_dir(root).map_err(|e| e.to_string())? {
                    let dir = item.map_err(|e| e.to_string())?.path();
                    if persistence::existing_dir(&dir).is_err() {
                        continue;
                    }
                    if let Ok(mut p) = persistence::read_publication(&dir) {
                        if p.receipt.format == "iceberg"
                            && matches!(p.status.as_str(), "staging" | "publishing")
                        {
                            p.status = "uncertain".into();
                            p.error=Some("Previous owner exited during storage operation; reconcile the retained receipt explicitly".into());
                            persistence::write_record(&dir.join("publication.json"), &p)?;
                        }
                    }
                }
            }
            Ok(())
        }
        pub fn checkpoint_stage(&mut self, request: CheckpointStage) -> Result<Value> {
            self.ensure_starting_allowed()?;
            self.status_with(&request.id, true)?;
            if self.storage_configuration.job.is_some() {
                return Err("One storage operation is already pending".into());
            }
            let profile = self.storage_configuration.configured()?;
            if profile.name != request.profile {
                return Err("Unknown storage profile".into());
            }
            let world = self.worlds.get(&request.id).ok_or("Unknown world")?;
            if world.generation != request.expected_generation
                || world
                    .instance
                    .as_ref()
                    .is_none_or(|i| i.backend.revision() != request.expected_revision)
            {
                return Err("Checkpoint generation/revision fence mismatch".into());
            }
            let (mut receipt, bytes) = persistence::freeze(world, &request.id, "iceberg")?;
            receipt.storage = Some(profile.location(&receipt.receipt_id));
            let root = persistence::checkpoint_root(&self.build_root, &request.id)?;
            persistence::private_dir(&root)?;
            let dir = root.join(&receipt.receipt_id);
            persistence::private_dir(&dir)?;
            File::open(root)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
            File::open(self.build_root.join(&request.id))
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
            self.launch_storage(
                profile,
                Publication {
                    status: "staging".into(),
                    receipt,
                    error: None,
                    staged: None,
                },
                dir,
                Work::Stage(bytes),
            )
        }
        pub fn checkpoint_publish(&mut self, request: CheckpointPublish) -> Result<Value> {
            self.ensure_starting_allowed()?;
            self.poll_storage();
            if self.storage_configuration.job.is_some() {
                return Err("One storage operation is already pending".into());
            }
            let profile = self.storage_configuration.configured()?;
            let receipt: Receipt =
                serde_json::from_value(request.receipt).map_err(|e| e.to_string())?;
            if !self.worlds.contains_key(&request.id) || receipt.origin.world_id != request.id {
                return Err("Publication must name its origin world".into());
            }
            let dir = persistence::receipt_dir(&self.build_root, &receipt)?;
            let mut p = persistence::read_publication(&dir)?;
            if p.receipt != receipt && stage_form(p.receipt.clone()) != receipt {
                return Err("Publication requires the exact retained receipt".into());
            }
            if !matches!(
                p.status.as_str(),
                "staged" | "uncertain" | "published" | "publishing"
            ) {
                return Err("Publication is not ready for explicit reconciliation".into());
            }
            let staged = staged(&p, &profile)?;
            p.status = "publishing".into();
            p.error = None;
            self.launch_storage(profile, p, dir, Work::Publish(staged))
        }
        fn launch_storage(
            &mut self,
            profile: Arc<Profile>,
            mut p: Publication,
            dir: PathBuf,
            work: Work,
        ) -> Result<Value> {
            persistence::write_record(&dir.join("publication.json"), &p)?;
            let ticket = json!({"schema_version":1,"id":p.receipt.origin.world_id,"receipt_id":p.receipt.receipt_id,"status":p.status});
            let cancel = Arc::new(AtomicBool::new(false));
            let stop = cancel.clone();
            let owner = self.shutdown.stopped.clone();
            let (sender, receiver) = mpsc::channel();
            let mut job = Job {
                world: p.receipt.origin.world_id.clone(),
                generation: p.receipt.origin.generation,
                receipt_id: p.receipt.receipt_id.clone(),
                phase: p.status.clone(),
                manifest: dir.join("publication.json"),
                cancel,
                receiver,
                thread: None,
            };
            job.thread = Some(
                std::thread::Builder::new()
                    .name("world-storage".into())
                    .spawn(move || {
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                run(
                                    profile.timeout_ms,
                                    || stop.load(Ordering::SeqCst) || owner.load(Ordering::SeqCst),
                                    async {
                                        match work {
                                            Work::Stage(bytes) => {
                                                let catalog = catalog(&profile, true).await?;
                                                let table = catalog
                                                    .load_table(&ident())
                                                    .await
                                                    .map_err(|e| e.to_string())?;
                                                let staged = iceberg_checkpoint::stage_bytes(
                                                    &table,
                                                    &p.receipt.receipt_id,
                                                    &p.receipt.storage.as_ref().unwrap().object_uri,
                                                    &bytes,
                                                )
                                                .await?;
                                                let location = p.receipt.storage.as_mut().unwrap();
                                                location.table_uuid =
                                                    Some(staged.table_uuid.clone());
                                                location.object_sha256 =
                                                    Some(staged.object_sha256.clone());
                                                location.envelope_sha256 =
                                                    Some(staged.checkpoint_sha256.clone());
                                                p.staged = Some(
                                                    serde_json::to_value(staged)
                                                        .map_err(|e| e.to_string())?,
                                                );
                                                p.status = "staged".into();
                                            }
                                            Work::Publish(staged) => {
                                                let catalog = catalog(&profile, false).await?;
                                                let published = iceberg_checkpoint::publish(
                                                    catalog.as_ref(),
                                                    &ident(),
                                                    &staged,
                                                )
                                                .await?;
                                                let location = p.receipt.storage.as_mut().unwrap();
                                                let snapshot = published.snapshot_id.to_string();
                                                if location
                                                    .snapshot_id
                                                    .as_ref()
                                                    .is_some_and(|s| *s != snapshot)
                                                {
                                                    return Err(
                                                        "Publication snapshot identity changed"
                                                            .into(),
                                                    );
                                                }
                                                location.snapshot_id = Some(snapshot);
                                                p.receipt
                                                    .published_at_unix_ms
                                                    .get_or_insert_with(super::super::timestamp);
                                                p.status = "published".into();
                                            }
                                        }
                                        Ok(())
                                    },
                                )
                            }))
                            .unwrap_or_else(|_| {
                                Err("Storage worker panicked; reconcile retained receipt".into())
                            });
                        if let Err(error) = &outcome {
                            p.status = "uncertain".into();
                            p.error = Some(error.clone());
                        }
                        let saved = persistence::write_record(&dir.join("publication.json"), &p);
                        let _ = sender.send(saved.and(outcome));
                    })
                    .map_err(|e| e.to_string())?,
            );
            self.storage_configuration.error = None;
            self.storage_configuration.uncertain = None;
            self.storage_configuration.job = Some(job);
            Ok(ticket)
        }
        pub(in crate::worlds) fn iceberg_restore_source(
            &self,
            p: &Publication,
        ) -> Result<RestoreSource> {
            if self.storage_configuration.job.as_ref().is_some_and(|job| {
                job.world == p.receipt.origin.world_id && job.receipt_id == p.receipt.receipt_id
            }) {
                return Err("Publication is still pending; poll checkpoint_status".into());
            }
            let profile = self.storage_configuration.configured()?;
            let staged = staged(p, &profile)?;
            let snapshot = p
                .receipt
                .storage
                .as_ref()
                .and_then(|s| s.snapshot_id.as_deref())
                .ok_or("Missing published Iceberg snapshot")?
                .parse::<i64>()
                .map_err(|_| "Invalid published Iceberg snapshot")?;
            Ok(RestoreSource::Iceberg(Read {
                profile,
                receipt: staged,
                snapshot,
            }))
        }
    }
}
