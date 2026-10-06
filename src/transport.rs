//! A catalog operation reads one immutable Hashtree directory throughout.
use anyhow::Result;
use hashtree_client::Cid;
use hashtree_client::{Client, ClientConfig, Reference};
use std::{path::Path, time::Duration};
use tokio::sync::OnceCell;

pub struct CatalogTransport {
    client: Client,
    reference: Reference,
    root: OnceCell<Cid>,
}

impl CatalogTransport {
    pub fn new(location: &str, cache: &Path) -> Result<Self> {
        let mut config = ClientConfig::from_env()?;
        // Include cold public-relay connections in the signed-root observation
        // window. Three seconds was too short for fresh installs in CI.
        config.resolve_window = Duration::from_secs(10);
        Self::with_config(location, cache, config)
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
