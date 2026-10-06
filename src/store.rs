use anyhow::{Result, ensure};
use async_trait::async_trait;
use futures::StreamExt;
use hashtree_core::{Hash, HashTree, HashTreeConfig, Store, StoreError};
use hashtree_fs::FsBlobStore;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const MAX_BLOCK: usize = 4 * 1024 * 1024;

pub struct VerifiedStore {
    inner: FsBlobStore,
    directory: PathBuf,
    remote: Option<reqwest::Url>,
    client: reqwest::Client,
    transport: Option<Arc<crate::transport::CatalogTransport>>,
}

impl VerifiedStore {
    pub fn new(path: &Path, remote: Option<reqwest::Url>) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            inner: FsBlobStore::new(path)?,
            directory: path.to_path_buf(),
            remote,
            transport: None,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        }))
    }
    pub fn hashtree(
        path: &Path,
        transport: Arc<crate::transport::CatalogTransport>,
    ) -> Result<Arc<Self>> {
        let mut store = Self::new(path, None)?;
        Arc::get_mut(&mut store).unwrap().transport = Some(transport);
        Ok(store)
    }
    pub fn tree(self: &Arc<Self>) -> HashTree<Self> {
        HashTree::new(HashTreeConfig::new(self.clone()))
    }
}

pub fn block_path(hash: &Hash) -> PathBuf {
    let hex = hex::encode(hash);
    PathBuf::from(&hex[..2]).join(&hex[2..4]).join(&hex[4..])
}

fn check(hash: &Hash, data: &[u8]) -> Result<(), StoreError> {
    if data.len() > MAX_BLOCK || Sha256::digest(data).as_slice() != hash {
        return Err(StoreError::Other(
            "content hash mismatch or oversized block".into(),
        ));
    }
    Ok(())
}

pub async fn download(client: &reqwest::Client, url: reqwest::Url, max: usize) -> Result<Vec<u8>> {
    let response = client.get(url).send().await?.error_for_status()?;
    ensure!(
        response.content_length().is_none_or(|n| n <= max as u64),
        "download exceeds size limit"
    );
    let mut stream = response.bytes_stream();
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        ensure!(
            data.len().saturating_add(chunk.len()) <= max,
            "download exceeds size limit"
        );
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

#[async_trait]
impl Store for VerifiedStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        check(&hash, &data)?;
        self.inner.put(hash, data).await
    }
    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let hex = hex::encode(hash);
        for path in [
            self.directory.join(block_path(hash)),
            self.directory.join(&hex[..2]).join(&hex[2..]),
        ] {
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if !metadata.is_file() || metadata.len() > MAX_BLOCK as u64 => {
                    return Err(StoreError::Other("invalid or oversized local block".into()));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        if let Some(bytes) = self.inner.get(hash).await? {
            check(hash, &bytes)?;
            return Ok(Some(bytes));
        }
        let relative = format!(
            "blobs/{}",
            block_path(hash).to_string_lossy().replace('\\', "/")
        );
        let bytes = if let Some(transport) = &self.transport {
            transport
                .read(&relative, MAX_BLOCK)
                .await
                .map_err(|e| StoreError::Other(e.to_string()))?
        } else if let Some(remote) = &self.remote {
            let url = remote
                .join(&relative)
                .map_err(|e| StoreError::Other(e.to_string()))?;
            download(&self.client, url, MAX_BLOCK)
                .await
                .map_err(|e| StoreError::Other(e.to_string()))?
        } else {
            return Ok(None);
        };
        check(hash, &bytes)?;
        self.inner.put(*hash, bytes.clone()).await?;
        Ok(Some(bytes))
    }
    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        Ok(self.get(hash).await?.is_some())
    }
    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.delete(hash).await
    }
}
