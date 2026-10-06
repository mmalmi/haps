//! A catalog operation reads one immutable Hashtree directory throughout.
use anyhow::Result;
use hashtree_client::Cid;
use hashtree_client::{Client, ClientConfig, Reference};
use std::path::Path;
use tokio::sync::OnceCell;

pub struct CatalogTransport {
    client: Client,
    reference: Reference,
    root: OnceCell<Cid>,
}

impl CatalogTransport {
    pub fn new(location: &str, cache: &Path) -> Result<Self> {
        Self::with_config(location, cache, ClientConfig::from_env()?)
    }

    pub fn with_config(location: &str, cache: &Path, config: ClientConfig) -> Result<Self> {
        Ok(Self {
            client: Client::new(config, &cache.join("transport"))?,
            reference: Reference::parse(location)?,
            root: OnceCell::new(),
        })
    }

    pub async fn read(&self, path: &str, limit: usize) -> Result<Vec<u8>> {
        let root = self
            .root
            .get_or_try_init(|| self.client.resolve(&self.reference))
            .await?;
        self.client.read_file(root, path, limit).await
    }
}
