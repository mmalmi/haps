//! Authenticate a portable event index, then expose it through nostr-pubsub.
use crate::{
    discovery::{Announcement, IndexHead},
    model::*,
    repository::Repository,
};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use hashtree_core::Cid;
use hashtree_index::{SearchIndex, SearchIndexOptions, SearchOptions};
use hashtree_nostr_pubsub::HashtreeNostrIndexEventBus;
use nostr::{Event, Filter};
use nostr_pubsub::{
    EventBus, EventSource, PublishReport, PubsubError, QueryEvent, QueryOptions, QueryReport,
    VerifiedEvent,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct IndexReader {
    location: String,
    author: String,
    home: PathBuf,
}
impl IndexReader {
    pub fn new(home: &Path, location: &str, author: &str) -> Self {
        Self {
            home: home.into(),
            location: location.into(),
            author: author.into(),
        }
    }
    async fn read(&self, filters: Vec<Filter>, options: QueryOptions) -> Result<QueryReport> {
        if self.location.starts_with("htree://") || self.location.starts_with("nhash") {
            let client = hashtree_client::Client::new(
                hashtree_client::ClientConfig::from_env()?,
                &self.home.join("cache/transport"),
            )?;
            let immutable =
                self.location.starts_with("htree://nhash") || self.location.starts_with("nhash");
            let root = if immutable {
                crate::event_catalog::decode_root(&self.location)?
            } else {
                let reference = hashtree_client::Reference::parse(&self.location)?;
                let (author, _) = reference.key.split_once('/').unwrap();
                ensure!(
                    nostr::PublicKey::parse(author)?.to_hex() == self.author,
                    "index publisher mismatch"
                );
                client.resolve(&reference).await?
            };
            let store = client.store();
            let index = hashtree_nostr::NostrEventStore::new(store.clone());
            let manifest = index.get_manifest(Some(&root)).await?;
            if manifest.by_id.is_some() {
                index.validate_index_root(Some(&root)).await?;
                return Ok(HashtreeNostrIndexEventBus::new(
                    store,
                    Some(root),
                    EventSource::local_index(&self.location),
                )
                .query(filters, options)
                .await?);
            }
            ensure!(!immutable, "root is not a Hashtree Nostr event index");
            // Older named Haps indexes are directories with signed index.json.
        }
        let repo = Repository::open(&self.location, &self.home.join("cache"))?;
        let event: Event = repo.metadata("index.json").await?;
        verify_event(&event)?;
        ensure!(
            event.pubkey.to_hex() == self.author
                && tag_value(&event, "d")? == "haps/discovery-index/v1",
            "index publisher mismatch"
        );
        let head: IndexHead = serde_json::from_str(&event.content)?;
        ensure!(
            head.schema == "haps.discovery.v1",
            "unsupported index schema"
        );
        let checkpoint = self.home.join("discovery/indexes").join(format!(
            "{}.json",
            hex::encode(Sha256::digest(
                format!("{}:{}", self.author, self.location).as_bytes()
            ))
        ));
        if checkpoint.exists() {
            let previous: Event = read_json(&checkpoint)?;
            let old: IndexHead = serde_json::from_str(&previous.content)?;
            ensure!(
                head.sequence > old.sequence
                    || (head.sequence == old.sequence && event.id == previous.id),
                "discovery index rollback or conflict"
            );
        }
        let source = EventSource::local_index(&self.location);
        let bus = HashtreeNostrIndexEventBus::new(
            repo.store.clone(),
            head.root.as_deref().map(Cid::parse).transpose()?,
            source.clone(),
        );
        let report = if filters.len() == 1 && filters[0].search.is_some() {
            let index = SearchIndex::new(repo.store.clone(), SearchIndexOptions::default());
            let root = head.search.as_deref().map(Cid::parse).transpose()?;
            let results = index
                .search(
                    root.as_ref(),
                    "",
                    filters[0].search.as_deref().unwrap(),
                    SearchOptions {
                        limit: options.limit,
                        full_match: false,
                    },
                )
                .await?;
            let mut events = Vec::new();
            for result in results {
                let announcement = Announcement::verify(repo.json(&result.value).await?)?;
                ensure!(
                    result.id == announcement.event.id.to_hex(),
                    "search index event ID mismatch"
                );
                if filters[0].match_event(&announcement.event, Default::default()) {
                    events.push(QueryEvent {
                        event: announcement.event.try_into()?,
                        source: source.clone(),
                        priority: 0,
                    });
                }
            }
            QueryReport { events }
        } else {
            bus.query(filters, options).await?
        };
        atomic_write(&checkpoint, &serde_json::to_vec(&event)?)?;
        Ok(report)
    }
}
#[async_trait]
impl EventBus for IndexReader {
    async fn publish(
        &self,
        _: VerifiedEvent,
        _: EventSource,
    ) -> nostr_pubsub::Result<PublishReport> {
        Err(PubsubError::Validation("shared index is read-only".into()))
    }
    async fn query(
        &self,
        filters: Vec<Filter>,
        options: QueryOptions,
    ) -> nostr_pubsub::Result<QueryReport> {
        // Hashtree's full-text traversal is a local future, as in the shared
        // Hashtree Nostr adapter. Keep it off the async router's worker threads.
        let reader = self.clone();
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || runtime.block_on(reader.read(filters, options)))
            .await
            .map_err(|error| PubsubError::Storage(error.to_string()))?
            .map_err(|error| PubsubError::Storage(format!("{error:#}")))
    }
}
