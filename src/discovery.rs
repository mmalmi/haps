//! Signed software announcements, a private Hashtree event cache, and relay outbox.
//! Index operators transport publisher signatures; they never become package authors.
use crate::{model::*, repository::Repository, store::VerifiedStore};
use anyhow::{Context, Result, ensure};
use hashtree_core::Cid;
use hashtree_index::{SearchIndex, SearchIndexOptions, SearchOptions};
use hashtree_nostr::{NostrEventStore, stored_event_from_nostr_sdk_event};
use hashtree_nostr_pubsub::HashtreeNostrIndexEventBus;
use nostr::{Event, EventBuilder, Filter, Keys, Kind, PublicKey, Tag};
use nostr_pubsub::{EventBus, EventSource, QueryOptions};
use nostr_pubsub_relay::RelayEventBus;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

pub const SOFTWARE_KIND: Kind = Kind::Custom(32267);
const LIMIT: usize = 2048;
const INDEX_ID: &str = "haps/discovery-index/v1";

#[derive(Clone, Debug)]
pub struct Announcement {
    pub event: Event,
    pub name: String,
    pub location: String,
}
impl Announcement {
    pub fn verify(event: Event) -> Result<Self> {
        event.verify()?;
        ensure!(
            event.kind == SOFTWARE_KIND && event.content.len() <= 16_384,
            "invalid software announcement"
        );
        let name = tag_value(&event, "d")?.to_owned();
        safe_name(&name)?;
        let location = tag_value(&event, "haps_catalog")?.to_owned();
        validate_location(&location)?;
        ensure!(
            !tag_value(&event, "name")?.is_empty(),
            "missing software name"
        );
        Ok(Self {
            event,
            name,
            location,
        })
    }
    pub fn sign(keys: &Keys, package: &PackageSpec, location: &str) -> Result<Event> {
        safe_name(&package.name)?;
        validate_location(location)?;
        let mut tags = vec![
            Tag::identifier(&package.name),
            Tag::parse(["name", &package.name])?,
            Tag::hashtag("haps"),
            Tag::parse(["haps_catalog", location])?,
        ];
        if let Some(source) = &package.source {
            tags.push(Tag::parse(["repository", &source.git])?);
        }
        let event = EventBuilder::new(SOFTWARE_KIND, &package.description)
            .tags(tags)
            .sign_with_keys(keys)?;
        Self::verify(event.clone())?;
        Ok(event)
    }
    pub fn author(&self) -> String {
        self.event.pubkey.to_hex()
    }
}

fn validate_location(location: &str) -> Result<()> {
    ensure!(location.len() <= 2048, "catalog location too long");
    if location.starts_with("htree://") {
        hashtree_client::Reference::parse(location)?;
    } else {
        let url = reqwest::Url::parse(location)?;
        ensure!(
            matches!(url.scheme(), "https" | "http")
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "discovered catalog must be an HTTP or Hashtree URL without credentials"
        );
    }
    Ok(())
}

fn retained(event: &Event) -> bool {
    if serde_json::to_vec(event).map_or(true, |bytes| bytes.len() > 32_768) {
        return false;
    }
    if event.created_at.as_secs() > nostr::Timestamp::now().as_secs().saturating_add(600) {
        return false;
    }
    if event.kind == SOFTWARE_KIND {
        return Announcement::verify(event.clone()).is_ok();
    }
    event.kind == APP_KIND && Release::verify(event.clone()).is_ok()
}

#[derive(Default, Serialize, Deserialize)]
struct CacheHead {
    root: Option<String>,
}

struct CacheState {
    root: Option<Cid>,
    store: std::sync::Arc<VerifiedStore>,
}
pub struct Discovery {
    home: PathBuf,
    // Serialize independent readers/writers before retiring a private generation.
    _guard: std::fs::File,
    cache: tokio::sync::Mutex<CacheState>,
}
fn generation(home: &Path, root: Option<&Cid>) -> PathBuf {
    home.join("discovery/roots").join(
        root.map(|cid| hex::encode(cid.hash))
            .unwrap_or_else(|| "empty".into()),
    )
}
async fn cached_events(state: &CacheState, filters: Vec<Filter>) -> Result<Vec<Event>> {
    let bus = HashtreeNostrIndexEventBus::new(
        state.store.clone(),
        state.root.clone(),
        EventSource::local_index("haps-private"),
    );
    Ok(bus
        .query(filters, QueryOptions { limit: Some(LIMIT) })
        .await?
        .events
        .into_iter()
        .map(|e| e.event.into_event())
        .collect())
}
impl Discovery {
    pub fn open(home: &Path) -> Result<Self> {
        let guard = lock(&home.join("discovery/cache.lock"))?;
        let path = home.join("discovery/root.json");
        let head: CacheHead = if path.exists() {
            read_json(&path)?
        } else {
            CacheHead::default()
        };
        let root = head.root.as_deref().map(Cid::parse).transpose()?;
        let store = VerifiedStore::new(&generation(home, root.as_ref()), None)?;
        Ok(Self {
            home: home.into(),
            _guard: guard,
            cache: tokio::sync::Mutex::new(CacheState { root, store }),
        })
    }
    pub async fn ingest(&self, events: impl IntoIterator<Item = Event>) -> Result<usize> {
        let mut state = self.cache.lock().await;
        let previous = cached_events(&state, vec![Filter::new()]).await?;
        let mut merged: BTreeMap<_, _> = previous
            .iter()
            .map(|event| (event.id, event.clone()))
            .collect();
        let mut count = 0;
        for event in events.into_iter().take(LIMIT) {
            if retained(&event) && !merged.contains_key(&event.id) {
                merged.insert(event.id, event);
                count += 1;
            }
        }
        if count == 0 {
            return Ok(0);
        }
        // Keep the newest addressable record before applying the retention cap.
        let mut latest = BTreeMap::<(nostr::PublicKey, Kind, String), Event>::new();
        for event in merged.into_values() {
            let key = (event.pubkey, event.kind, tag_value(&event, "d")?.to_owned());
            if latest.get(&key).is_none_or(|old| newer(&event, old)) {
                latest.insert(key, event);
            }
        }
        let mut events: Vec<_> = latest.into_values().collect();
        events.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        events.truncate(LIMIT);
        if events
            .iter()
            .map(|e| e.id)
            .collect::<std::collections::BTreeSet<_>>()
            == previous.iter().map(|e| e.id).collect()
        {
            return Ok(0);
        }
        let roots = self.home.join("discovery/roots");
        fs::create_dir_all(&roots)?;
        let staging = tempfile::tempdir_in(&roots)?;
        let store = VerifiedStore::new(staging.path(), None)?;
        let root = NostrEventStore::new(store)
            .build(None, events.iter().map(stored_event_from_nostr_sdk_event))
            .await?;
        let destination = generation(&self.home, root.as_ref());
        if !destination.exists() {
            fs::rename(staging.path(), &destination)?;
        }
        let next = CacheState {
            store: VerifiedStore::new(&destination, None)?,
            root: root.clone(),
        };
        let verified = cached_events(&next, vec![Filter::new()]).await?;
        ensure!(
            verified
                .iter()
                .map(|e| e.id)
                .collect::<std::collections::BTreeSet<_>>()
                == events.iter().map(|e| e.id).collect(),
            "new discovery generation failed validation"
        );
        atomic_write(
            &self.home.join("discovery/root.json"),
            &serde_json::to_vec(&CacheHead {
                root: root.as_ref().map(ToString::to_string),
            })?,
        )?;
        let old = generation(&self.home, state.root.as_ref());
        *state = next;
        // Only regenerable private event-cache blocks are collected, after commit.
        if old != destination && old.exists() {
            fs::remove_dir_all(old)?;
        }
        Ok(count)
    }
    pub async fn events(&self, filters: Vec<Filter>) -> Result<Vec<Event>> {
        cached_events(&*self.cache.lock().await, filters).await
    }
    pub async fn announcements(&self, publisher: Option<PublicKey>) -> Result<Vec<Announcement>> {
        let mut filter = Filter::new().kind(SOFTWARE_KIND);
        if let Some(publisher) = publisher {
            filter = filter.author(publisher);
        }
        let mut latest = BTreeMap::<(String, String), Announcement>::new();
        for event in self.events(vec![filter]).await? {
            let announcement = Announcement::verify(event)?;
            let key = (announcement.author(), announcement.name.clone());
            if latest
                .get(&key)
                .is_none_or(|old| newer(&announcement.event, &old.event))
            {
                latest.insert(key, announcement);
            }
        }
        Ok(latest.into_values().collect())
    }
    /// Bounded observation, never a proof of absence. The subscription stays open
    /// through EOSE so a relay's initial response does not end the observation.
    pub async fn refresh(
        &self,
        bus: &RelayEventBus,
        publisher: Option<PublicKey>,
        window: Duration,
    ) -> Result<usize> {
        use nostr_pubsub::NostrEventSubscriber;
        let mut filter = Filter::new()
            .kind(SOFTWARE_KIND)
            .hashtag("haps")
            .limit(LIMIT);
        if let Some(publisher) = publisher {
            filter = filter.author(publisher);
        }
        let (sender, mut receiver) = tokio::sync::mpsc::channel(LIMIT);
        let subscription = bus
            .subscribe(
                vec![filter],
                std::sync::Arc::new(move |event| {
                    let _ = sender.try_send(event.event.into_event());
                }),
            )
            .await?;
        let deadline = tokio::time::sleep(window);
        tokio::pin!(deadline);
        let mut events = BTreeMap::new();
        loop {
            tokio::select! {
                ()=&mut deadline => break,
                event=receiver.recv() => match event {
                    Some(event) => {events.insert(event.id,event); if events.len() >= LIMIT {break;}},
                    None => break,
                }
            }
        }
        drop(subscription);
        self.ingest(events.into_values()).await
    }
    pub fn queue(&self, events: &[Event]) -> Result<()> {
        let dir = self.home.join("discovery/outbox");
        fs::create_dir_all(&dir)?;
        ensure!(
            fs::read_dir(&dir)?.count().saturating_add(events.len()) <= 10_000,
            "publication outbox is full; run haps sync"
        );
        for event in events {
            ensure!(retained(event), "unsupported discovery event");
            atomic_write(
                &dir.join(format!("{}.json", event.id)),
                &serde_json::to_vec(event)?,
            )?;
        }
        Ok(())
    }
    pub async fn flush(&self, bus: &RelayEventBus) -> Result<usize> {
        bus.client()
            .wait_for_connection(Duration::from_secs(3))
            .await;
        let dir = self.home.join("discovery/outbox");
        if !dir.exists() {
            return Ok(0);
        }
        let mut pending = 0;
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            let event: Event = read_json(&path)?;
            ensure!(retained(&event), "invalid queued event");
            // EventBus::publish only acknowledges an in-memory send queue.
            // Keep the durable copy until an actual relay sends a positive OK.
            let result = tokio::time::timeout(
                Duration::from_secs(10),
                bus.client()
                    .send_event_to(bus.relays().iter().map(String::as_str), &event),
            )
            .await;
            if matches!(result, Ok(Ok(ref output)) if !output.success.is_empty()) {
                fs::remove_file(path)?;
            } else {
                eprintln!("Announcement {} remains queued: {:?}", event.id, result);
                pending += 1;
            }
        }
        Ok(pending)
    }
    pub async fn import_index(&self, location: &str, author: &str) -> Result<usize> {
        self.lookup_index(location, author, None, None).await
    }
    pub async fn lookup_index(
        &self,
        location: &str,
        author: &str,
        query: Option<&str>,
        publisher: Option<PublicKey>,
    ) -> Result<usize> {
        let repo = Repository::open(location, &self.home.join("cache"))?;
        let event: Event = repo.metadata("index.json").await?;
        verify_event(&event)?;
        ensure!(
            event.pubkey.to_hex() == author && tag_value(&event, "d")? == INDEX_ID,
            "index publisher mismatch"
        );
        let head: IndexHead = serde_json::from_str(&event.content)?;
        ensure!(
            head.schema == "haps.discovery.v1",
            "unsupported index schema"
        );
        let checkpoint = self.home.join("discovery/indexes").join(format!(
            "{}.json",
            hex::encode(sha2::Sha256::digest(
                format!("{author}:{location}").as_bytes()
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
        let events = if let Some(query) = query {
            let index = SearchIndex::new(repo.store.clone(), SearchIndexOptions::default());
            let root = head.search.as_deref().map(Cid::parse).transpose()?;
            let results = index
                .search(
                    root.as_ref(),
                    "",
                    query,
                    SearchOptions {
                        limit: Some(100),
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
                if publisher.is_none_or(|key| announcement.event.pubkey == key) {
                    events.push(announcement.event);
                }
            }
            events
        } else {
            let bus = HashtreeNostrIndexEventBus::new(
                repo.store.clone(),
                head.root.as_deref().map(Cid::parse).transpose()?,
                EventSource::local_index(location),
            );
            let mut filter = Filter::new().kind(SOFTWARE_KIND);
            if let Some(publisher) = publisher {
                filter = filter.author(publisher);
            }
            bus.query(vec![filter], QueryOptions { limit: Some(LIMIT) })
                .await?
                .events
                .into_iter()
                .map(|e| e.event.into_event())
                .collect()
        };
        let count = self.ingest(events).await?;
        atomic_write(&checkpoint, &serde_json::to_vec(&event)?)?;
        Ok(count)
    }
    pub async fn export_index(&self, out: &Path, keys: &Keys) -> Result<()> {
        let _guard = lock(&out.join("index.lock"))?;
        let previous = out.join("index.json");
        let root = self.cache.lock().await.root.clone();
        let sequence = if previous.exists() {
            let event: Event = read_json(&previous)?;
            verify_event(&event)?;
            ensure!(
                event.pubkey == keys.public_key(),
                "index belongs to another publisher"
            );
            let head: IndexHead = serde_json::from_str(&event.content)?;
            ensure!(
                tag_value(&event, "d")? == INDEX_ID && head.schema == "haps.discovery.v1",
                "invalid previous index head"
            );
            if head.root == root.as_ref().map(ToString::to_string) {
                return Ok(());
            }
            head.sequence
                .checked_add(1)
                .context("index sequence overflow")?
        } else {
            1
        };
        let state = self.cache.lock().await;
        let root = state.root.clone();
        // Build an immutable export generation, then swap only its signed head.
        copy_blocks(&generation(&self.home, root.as_ref()), &out.join("blobs"))?;
        drop(state);
        let repo = Repository::local(out.to_path_buf())?;
        let index = SearchIndex::new(repo.store.clone(), SearchIndexOptions::default());
        let mut search = None;
        for announcement in self.announcements(None).await? {
            let cid = repo.put_json(&announcement.event).await?;
            let terms = index.parse_keywords(&format!(
                "{} {}",
                announcement.name, announcement.event.content
            ));
            search = Some(
                index
                    .index(
                        search.as_ref(),
                        "",
                        &terms,
                        &announcement.event.id.to_hex(),
                        &cid,
                    )
                    .await?,
            );
        }
        let event = sign(
            keys,
            INDEX_ID,
            &IndexHead {
                schema: "haps.discovery.v1".into(),
                sequence,
                root: root.map(|c| c.to_string()),
                search: search.map(|c| c.to_string()),
            },
        )?;
        atomic_write(&previous, &serde_json::to_vec(&event)?)
    }
}
use sha2::Digest;
#[derive(Serialize, Deserialize)]
struct IndexHead {
    schema: String,
    sequence: u64,
    root: Option<String>,
    #[serde(default)]
    search: Option<String>,
}
fn newer(a: &Event, b: &Event) -> bool {
    a.created_at > b.created_at || (a.created_at == b.created_at && a.id < b.id)
}
fn copy_blocks(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    if !from.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            copy_blocks(&entry.path(), &to.join(entry.file_name()))?;
        } else if entry.file_type()?.is_file() {
            fs::copy(entry.path(), to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// Reuse the shared daemon when available, respecting its local-only setting.
pub async fn relay_bus(home: &Path) -> Result<RelayEventBus> {
    let config = hashtree_client::ClientConfig::from_env()?;
    let client = hashtree_client::Client::new(config.clone(), &home.join("cache/transport"))?;
    let relays = if let Some(daemon) = client.daemon_url().await {
        vec![format!("{}/ws", daemon.replacen("http://", "ws://", 1))]
    } else {
        ensure!(!config.local_only, "local Hashtree daemon is unavailable");
        config.relays
    };
    let bus = RelayEventBus::new(relays, Duration::from_secs(3)).await?;
    bus.client()
        .wait_for_connection(Duration::from_secs(3))
        .await;
    Ok(bus)
}
