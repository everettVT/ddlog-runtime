//! Bounded real-R2 acceptance adapter using existing Cloudflare OAuth/API auth.
//! Not a throughput driver: buffers <=2MiB per object and reads ranges from a
//! freshly fetched remote object. No local object cache or mock is used.
use async_trait::async_trait;
use bytes::Bytes;
use futures::{stream::BoxStream, StreamExt};
use iceberg::io::{
    FileMetadata, FileRead, FileWrite, InputFile, OutputFile, Storage, StorageConfig,
    StorageFactory,
};
use iceberg::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::{ops::Range, sync::Arc};
const MAX: usize = 2 * 1024 * 1024;
fn fail(message: &str) -> Error {
    Error::new(ErrorKind::Unexpected, message)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct R2Rest {
    pub account: String,
    pub bucket: String,
    pub prefix: String,
}
impl R2Rest {
    fn url(&self, path: &str) -> Result<String> {
        let base = format!("r2://{}/", self.bucket);
        let key = path
            .strip_prefix(&base)
            .ok_or_else(|| fail("R2 URI bucket mismatch"))?;
        if self.bucket != "archetype-staging"
            || !self.prefix.starts_with("ddlog-acceptance/")
            || !key.starts_with(&format!("{}/", self.prefix))
            || key.contains("..")
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))
        {
            return Err(fail("R2 acceptance scope rejected"));
        }
        Ok(format!(
            "https://api.cloudflare.com/client/v4/accounts/{}/r2/buckets/{}/objects/{}",
            self.account, self.bucket, key
        ))
    }
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Bytes>,
    ) -> Result<reqwest::Response> {
        let token = std::env::var("CLOUDFLARE_API_TOKEN")
            .map_err(|_| fail("CLOUDFLARE_API_TOKEN required"))?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|_| fail("R2 HTTP client failed"))?;
        let mut request = client.request(method, self.url(path)?).bearer_auth(token);
        if let Some(bytes) = body {
            if bytes.len() > MAX {
                return Err(fail("R2 acceptance object limit"));
            }
            request = request
                .header("Content-Type", "application/octet-stream")
                .body(bytes);
        }
        request
            .send()
            .await
            .map_err(|_| fail("R2 request failed (details suppressed to protect credentials)"))
    }
}
#[typetag::serde]
impl StorageFactory for R2Rest {
    fn build(&self, _: &StorageConfig) -> Result<Arc<dyn Storage>> {
        Ok(Arc::new(self.clone()))
    }
}
#[typetag::serde]
#[async_trait]
impl Storage for R2Rest {
    async fn exists(&self, path: &str) -> Result<bool> {
        let response = self.request(reqwest::Method::GET, path, None).await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !response.status().is_success() {
            return Err(fail("R2 existence request rejected"));
        }
        Ok(true)
    }
    async fn metadata(&self, path: &str) -> Result<FileMetadata> {
        Ok(FileMetadata {
            size: self.read(path).await?.len() as u64,
        })
    }
    async fn read(&self, path: &str) -> Result<Bytes> {
        let mut response = self.request(reqwest::Method::GET, path, None).await?;
        if !response.status().is_success() {
            return Err(fail("R2 read rejected"));
        }
        let mut out = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| fail("R2 read failed"))? {
            if out.len() + chunk.len() > MAX {
                return Err(fail("R2 acceptance read limit"));
            }
            out.extend_from_slice(&chunk)
        }
        Ok(Bytes::from(out))
    }
    async fn reader(&self, path: &str) -> Result<Box<dyn FileRead>> {
        Ok(Box::new(R2Reader {
            storage: self.clone(),
            path: path.into(),
        }))
    }
    async fn write(&self, path: &str, bs: Bytes) -> Result<()> {
        let response = self.request(reqwest::Method::PUT, path, Some(bs)).await?;
        if !response.status().is_success() {
            return Err(fail("R2 write rejected"));
        }
        Ok(())
    }
    async fn writer(&self, path: &str) -> Result<Box<dyn FileWrite>> {
        self.url(path)?;
        Ok(Box::new(R2Writer {
            storage: self.clone(),
            path: path.into(),
            bytes: Some(Vec::new()),
        }))
    }
    async fn delete(&self, path: &str) -> Result<()> {
        let response = self.request(reqwest::Method::DELETE, path, None).await?;
        if !response.status().is_success() {
            return Err(fail("R2 deletion rejected"));
        }
        Ok(())
    }
    async fn delete_prefix(&self, _: &str) -> Result<()> {
        Err(fail("Broad prefix deletion disabled"))
    }
    async fn delete_stream(&self, mut paths: BoxStream<'static, String>) -> Result<()> {
        while let Some(path) = paths.next().await {
            self.delete(&path).await?
        }
        Ok(())
    }
    fn new_input(&self, path: &str) -> Result<InputFile> {
        self.url(path)?;
        Ok(InputFile::new(Arc::new(self.clone()), path.into()))
    }
    fn new_output(&self, path: &str) -> Result<OutputFile> {
        self.url(path)?;
        Ok(OutputFile::new(Arc::new(self.clone()), path.into()))
    }
}
struct R2Reader {
    storage: R2Rest,
    path: String,
}
#[async_trait]
impl FileRead for R2Reader {
    async fn read(&self, r: Range<u64>) -> Result<Bytes> {
        let b = self.storage.read(&self.path).await?;
        if r.start > r.end || r.end > b.len() as u64 {
            return Err(fail("R2 range invalid"));
        }
        Ok(b.slice(r.start as usize..r.end as usize))
    }
}
struct R2Writer {
    storage: R2Rest,
    path: String,
    bytes: Option<Vec<u8>>,
}
#[async_trait]
impl FileWrite for R2Writer {
    async fn write(&mut self, bs: Bytes) -> Result<()> {
        let b = self.bytes.as_mut().ok_or_else(|| fail("writer closed"))?;
        if b.len() + bs.len() > MAX {
            return Err(fail("R2 buffer limit"));
        }
        b.extend_from_slice(&bs);
        Ok(())
    }
    async fn close(&mut self) -> Result<()> {
        let b = self.bytes.take().ok_or_else(|| fail("writer closed"))?;
        self.storage.write(&self.path, Bytes::from(b)).await
    }
}
