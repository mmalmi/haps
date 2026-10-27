//! Progressive search: each update is a complete view of verified package heads
//! under the current social policy, never an append-only list of old results.
use super::*;
use futures::{
    FutureExt, StreamExt,
    stream::{self, LocalBoxStream, SelectAll},
};
use haps::{
    discovery::SOFTWARE_KIND,
    event_catalog::{INDEX_KIND, IndexAnnouncement},
    lookup::Lookup,
};
use nostr::{Filter, PublicKey};
use std::{io::Write, time::Duration};

const LIMIT: usize = 2048;

fn newer(event: &Event, old: &Event) -> bool {
    event.created_at > old.created_at || (event.created_at == old.created_at && event.id < old.id)
}

fn matches(query: &str, name: &str, description: &str) -> bool {
    let text = format!("{name} {description}").to_lowercase();
    query
        .split_whitespace()
        .all(|word| text.contains(&word.to_lowercase()))
}

#[derive(Default)]
struct Results {
    heads: BTreeMap<(String, String), Announcement>,
    events: BTreeMap<EventId, Event>,
    indexes: BTreeMap<(String, String), IndexAnnouncement>,
    catalogs: BTreeMap<(String, String), Vec<Release>>,
    seen: BTreeSet<EventId>,
}

impl Results {
    fn ingest(&mut self, events: &[Event], trust: &mut Trust) {
        for event in events {
            if self.seen.len() >= 10_000 || self.seen.contains(&event.id) {
                continue;
            }
            if event.verify().is_err()
                || event.created_at.as_secs() > Timestamp::now().as_secs().saturating_add(300)
                || serde_json::to_vec(event).map_or(true, |bytes| bytes.len() > 32_768)
            {
                continue;
            }
            self.seen.insert(event.id);
            if let Ok(head) = Announcement::verify(event.clone()) {
                let key = (head.author(), head.name.clone());
                if self
                    .heads
                    .get(&key)
                    .is_none_or(|old| newer(event, &old.event))
                {
                    self.heads.insert(key, head);
                }
            } else if let Ok(index) = IndexAnnouncement::verify(event.clone()) {
                let key = (event.pubkey.to_hex(), tag_value(event, "d").unwrap().into());
                if self
                    .indexes
                    .get(&key)
                    .is_none_or(|old| newer(event, &old.event))
                {
                    self.indexes.insert(key, index);
                }
            } else if Release::verify(event.clone()).is_ok() {
                self.events.insert(event.id, event.clone());
            } else if (event.kind == Kind::ContactList
                || event.kind == Kind::MuteList
                || parse_attestation(event).is_ok())
                && let Err(error) = trust.ingest(event.clone())
            {
                eprintln!("Invalid social event: {error:#}");
            }
        }
    }

    fn releases(&self, query: &str) -> Result<Vec<Release>> {
        let mut found = BTreeMap::new();
        for announcement in self.heads.values() {
            let Some(head) = &announcement.head else {
                continue;
            };
            if !matches(query, &announcement.name, &head.description) {
                continue;
            }
            for pointer in &head.releases {
                let id = EventId::from_hex(&pointer.id)?;
                if let Some(event) = self.events.get(&id) {
                    let release = head.verify_release(
                        pointer,
                        event.clone(),
                        &announcement.author(),
                        &announcement.name,
                    )?;
                    found.insert(release.coordinate(), release);
                }
            }
        }
        for release in self.catalogs.values().flatten() {
            // The latest signed direct head is authoritative for this package.
            // A slower legacy catalog must not restore releases it has replaced.
            if self
                .heads
                .get(&(release.author(), release.data.package.name.clone()))
                .is_some_and(|head| head.head.is_some())
            {
                continue;
            }
            if !matches(
                query,
                &release.data.package.name,
                &release.data.package.description,
            ) {
                continue;
            }
            if let Some(old) = found.insert(release.coordinate(), release.clone()) {
                ensure!(
                    old.event.id == release.event.id,
                    "conflicting signed releases at {}",
                    release.coordinate()
                );
            }
        }
        Ok(found.into_values().collect())
    }

    fn visible(&self, query: &str, trust: &Trust) -> Result<Vec<Release>> {
        let mut releases = self.releases(query)?;
        releases.retain(|release| trust.socially_trusted(release));
        releases.sort_by_key(|release| {
            (
                trust.distance(&release.author()).unwrap_or(u32::MAX),
                std::cmp::Reverse(trust.attesters(release).len()),
                release.identity(),
                std::cmp::Reverse(release.data.package.version.clone()),
                release.event.id,
            )
        });
        Ok(releases)
    }

    fn sources(&self, config: &mut Config, trust: &Trust, query: &str) -> Result<()> {
        config.indexes.retain(|name, source| {
            !name.starts_with("social-")
                || (!trust.muted(&source.author)
                    && !trust.overmuted(&source.author)
                    && trust.distance(&source.author).is_some_and(|d| d <= 2))
        });
        for index in self.indexes.values() {
            let author = index.event.pubkey.to_hex();
            if trust.muted(&author)
                || trust.overmuted(&author)
                || !trust.distance(&author).is_some_and(|d| d <= 2)
            {
                continue;
            }
            let name = format!(
                "social-{author}-{}",
                hex::encode(tag_value(&index.event, "d")?)
            );
            config.indexes.insert(
                name,
                Source {
                    location: index.location.clone(),
                    author,
                    sequence: 0,
                    event_id: String::new(),
                },
            );
        }
        for announcement in self.heads.values() {
            if announcement.head.is_some()
                || !matches(query, &announcement.name, &announcement.event.content)
                || config.sources.values().any(|source| {
                    source.author == announcement.author()
                        && source.location == announcement.location
                })
            {
                continue;
            }
            config
                .sources
                .entry(format!(
                    "discovered-{}-{}",
                    announcement.author(),
                    announcement.name
                ))
                .or_insert_with(|| Source {
                    location: announcement.location.clone(),
                    author: announcement.author(),
                    sequence: 0,
                    event_id: String::new(),
                })
                .location = announcement.location.clone();
        }
        Ok(())
    }
}

enum Update {
    Events(Vec<Event>),
    Catalog {
        author: String,
        location: String,
        snapshot: Box<Snapshot>,
        releases: Vec<Release>,
    },
    Relay(Arc<nostr_pubsub_relay::RelayEventBus>),
}

#[derive(Default)]
struct Pending {
    streams: SelectAll<LocalBoxStream<'static, Result<Update>>>,
    lookups: Vec<(Lookup, Arc<tokio::sync::Semaphore>)>,
    queries: Vec<Vec<Filter>>,
    indexes: BTreeSet<(String, String)>,
    catalogs: BTreeSet<(String, String)>,
    social: BTreeSet<PublicKey>,
    index_authors: BTreeSet<PublicKey>,
    pointers: BTreeSet<String>,
    fetches: BTreeMap<EventId, futures::future::AbortHandle>,
    feedback: BTreeSet<EventId>,
}

impl Pending {
    fn enqueue(
        &mut self,
        updates: LocalBoxStream<'static, nostr_pubsub::Result<Vec<Event>>>,
        permits: Arc<tokio::sync::Semaphore>,
    ) {
        self.streams.push(
            stream::once(async move {
                let permit = permits
                    .acquire_owned()
                    .await
                    .expect("search semaphore remains open");
                updates
                    .map(move |result| {
                        let _permit = &permit;
                        result.map(Update::Events).map_err(Into::into)
                    })
                    .boxed_local()
            })
            .flatten()
            .boxed_local(),
        );
    }

    fn lookup(&mut self, lookup: Lookup) {
        // Bound requests to each source independently. An offline index must
        // never occupy the slots needed by another index or the live relays.
        let permits = Arc::new(tokio::sync::Semaphore::new(4));
        for filters in self.queries.clone() {
            self.enqueue(lookup.query_updates(filters), permits.clone());
        }
        self.lookups.push((lookup, permits));
    }

    fn query(&mut self, filters: Vec<Filter>) {
        let updates: Vec<_> = self
            .lookups
            .iter()
            .map(|(lookup, permits)| (lookup.query_updates(filters.clone()), permits.clone()))
            .collect();
        for (updates, permits) in updates {
            self.enqueue(updates, permits);
        }
        self.queries.push(filters);
    }

    fn plan(
        &mut self,
        home: &Path,
        config: &Config,
        trust: &Trust,
        results: &Results,
        query: &str,
        permits: &Arc<tokio::sync::Semaphore>,
    ) -> Result<()> {
        for source in config.indexes.values() {
            if self.indexes.len() >= 256 {
                break;
            }
            if !self
                .indexes
                .insert((source.author.clone(), source.location.clone()))
            {
                continue;
            }
            let reader = Arc::new(haps::index_reader::IndexReader::new(
                home,
                &source.location,
                &source.author,
            ));
            self.lookup(
                Lookup::default()
                    .index(&format!("{}:{}", source.author, source.location), reader)?,
            );
        }
        let remaining = 256usize.saturating_sub(self.social.len());
        let authors: Vec<_> = trust
            .discovery_authors(1)
            .into_iter()
            .filter(|author| self.social.insert(*author))
            .take(remaining)
            .collect();
        if !authors.is_empty() {
            self.query(vec![
                Filter::new()
                    .kinds([Kind::ContactList, Kind::MuteList])
                    .authors(authors)
                    .limit(512),
            ]);
        }
        let remaining = 256usize.saturating_sub(self.index_authors.len());
        let authors: Vec<_> = trust
            .discovery_authors(2)
            .into_iter()
            .filter(|author| self.index_authors.insert(*author))
            .take(remaining)
            .collect();
        if !authors.is_empty() {
            self.query(vec![
                Filter::new()
                    .kinds([INDEX_KIND, APP_KIND])
                    .authors(authors)
                    .custom_tag(
                        nostr::SingleLetterTag::lowercase(nostr::Alphabet::L),
                        "hashtree",
                    )
                    .limit(256),
            ]);
        }
        let mut ids = Vec::new();
        let mut findings = Vec::new();
        for announcement in results.heads.values() {
            let Some(head) = &announcement.head else {
                continue;
            };
            if !matches(query, &announcement.name, &head.description) {
                continue;
            }
            for pointer in &head.releases {
                if self.pointers.len() >= LIMIT {
                    break;
                }
                if !self.pointers.insert(pointer.id.clone()) {
                    continue;
                }
                let id = EventId::from_hex(&pointer.id)?;
                if self.feedback.len() < LIMIT && self.feedback.insert(id) {
                    findings.push(id.to_hex());
                }
                if results.events.contains_key(&id) {
                    continue;
                }
                ids.push(id);
                let location = head.payload.clone();
                let cid = pointer.cid.clone();
                let home = home.to_path_buf();
                let permits = permits.clone();
                let (abort, registration) = futures::future::AbortHandle::new_pair();
                self.fetches.insert(id, abort);
                let fetch = futures::future::Abortable::new(
                    async move {
                        let _permit = permits.acquire().await?;
                        let repo = Repository::open(&location, &home.join("cache"))?;
                        let event: Event = repo.json(&cid).await?;
                        ensure!(
                            event.id == id,
                            "package pointer returned a different release"
                        );
                        Ok(Update::Events(vec![event]))
                    },
                    registration,
                );
                self.streams.push(
                    stream::once(async move {
                        fetch
                            .await
                            .unwrap_or_else(|_| Ok(Update::Events(Vec::new())))
                    })
                    .boxed_local(),
                );
            }
        }
        for ids in ids.chunks(128) {
            self.query(vec![Filter::new().ids(ids.to_vec()).limit(LIMIT)]);
        }
        let remaining = LIMIT.saturating_sub(self.feedback.len());
        findings.extend(
            results
                .releases(query)?
                .into_iter()
                .filter(|release| self.feedback.insert(release.event.id))
                .take(remaining)
                .map(|release| release.event.id.to_hex()),
        );
        for ids in findings.chunks(128) {
            self.query(vec![
                Filter::new()
                    .kind(Kind::from(nostr_identity::FACT_SNAPSHOT_KIND))
                    .identifiers(ids.to_vec())
                    .limit(512),
                Filter::new()
                    .kind(APP_KIND)
                    .identifiers(ids.iter().map(|id| format!("haps/attestation/{id}")))
                    .limit(512),
            ]);
        }
        for source in config.sources.values() {
            if !self
                .catalogs
                .insert((source.author.clone(), source.location.clone()))
            {
                continue;
            }
            let (author, location) = (source.author.clone(), source.location.clone());
            let home = home.to_path_buf();
            let query = query.to_owned();
            let permits = permits.clone();
            self.streams.push(
                stream::once(async move {
                    let _permit = permits.acquire().await?;
                    let repo = Repository::open(&location, &home.join("cache"))?;
                    let snapshot = repo.catalog(&author).await?;
                    let releases = if query.trim().is_empty() {
                        repo.releases(&snapshot).await?
                    } else {
                        repo.search(&snapshot, &query).await?
                    };
                    Ok(Update::Catalog {
                        author,
                        location,
                        snapshot: Box::new(snapshot),
                        releases,
                    })
                })
                .boxed_local(),
            );
        }
        Ok(())
    }
}

fn emit(
    results: &Results,
    query: &str,
    trust: &Trust,
    config: &Config,
    previous: &mut Option<String>,
    output: &mut impl Write,
) -> Result<()> {
    let rows: Vec<_> = results
        .visible(query, trust)?
        .iter()
        .map(|release| release_json(release, trust, &config.aliases))
        .collect();
    let snapshot = serde_json::to_string(&rows)?;
    if previous.as_ref() != Some(&snapshot) {
        writeln!(output, "{snapshot}")?;
        output.flush()?;
        *previous = Some(snapshot);
    }
    Ok(())
}

pub(super) async fn run(
    home: &Path,
    config: &mut Config,
    trust: &mut Trust,
    query: &str,
    json: bool,
    streaming: bool,
) -> Result<()> {
    let discovery = Discovery::open(home)?;
    let cached = discovery.events(vec![Filter::new()]).await?;
    let mut results = Results::default();
    results.ingest(&cached, trust);
    let mut previous = None;
    if streaming {
        emit(
            &results,
            query,
            trust,
            config,
            &mut previous,
            &mut std::io::stdout(),
        )?;
    }
    let mut pending = Pending::default();
    // Do not wait for relay setup before starting independent index readers.
    if networking_enabled(config) {
        let home = home.to_path_buf();
        pending.streams.push(
            stream::once(async move {
                Ok(Update::Relay(Arc::new(
                    haps::discovery::relay_bus(&home).await?,
                )))
            })
            .boxed_local(),
        );
    }
    pending.query(vec![
        Filter::new()
            .kind(SOFTWARE_KIND)
            .hashtag("haps")
            .limit(LIMIT),
    ]);
    let permits = Arc::new(tokio::sync::Semaphore::new(8));
    results.sources(config, trust, query)?;
    pending.plan(home, config, trust, &results, query, &permits)?;
    while let Some(update) = pending.streams.next().await {
        // Drain only already-ready work: no quiet timer and no wait for another
        // source. This coalesces relay bursts before planning follow-up queries.
        let mut updates = vec![update];
        for _ in 0..127 {
            match pending.streams.next().now_or_never() {
                Some(Some(update)) => updates.push(update),
                _ => break,
            }
        }
        let mut events = BTreeMap::new();
        for update in updates {
            match update {
                Ok(Update::Events(batch)) => {
                    events.extend(batch.into_iter().map(|event| (event.id, event)))
                }
                Ok(Update::Catalog {
                    author,
                    location,
                    snapshot,
                    releases,
                }) => {
                    for source in config
                        .sources
                        .values_mut()
                        .filter(|source| source.author == author && source.location == location)
                    {
                        check_checkpoint(source, &snapshot)?;
                    }
                    results.catalogs.insert((author, location), releases);
                }
                Ok(Update::Relay(bus)) => {
                    pending.lookup(Lookup::default().relays(bus, Duration::from_secs(3))?)
                }
                Err(error) => {
                    eprintln!("Search source unavailable; results may be incomplete: {error:#}")
                }
            }
        }
        let events: Vec<_> = events
            .into_values()
            .filter(|event| !results.seen.contains(&event.id))
            .collect();
        results.ingest(&events, trust);
        // A relay/index response makes its slower block fallback redundant.
        for event in &events {
            if results.events.contains_key(&event.id)
                && let Some(fetch) = pending.fetches.remove(&event.id)
            {
                fetch.abort();
            }
        }
        // Emit before persistence; disk writes must not hide ready rows.
        if streaming {
            emit(
                &results,
                query,
                trust,
                config,
                &mut previous,
                &mut std::io::stdout(),
            )?;
        }
        if !events.is_empty() {
            discovery.ingest(events).await?;
            save_trust(home, trust)?;
        }
        results.sources(config, trust, query)?;
        pending.plan(home, config, trust, &results, query, &permits)?;
    }
    save_config(home, config)?;
    if !streaming {
        if json {
            emit(
                &results,
                query,
                trust,
                config,
                &mut previous,
                &mut std::io::stdout(),
            )?;
        } else {
            for release in results.visible(query, trust)? {
                print_release(&release, trust, &config.aliases)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use haps::package_head::{PackageHead, ReleasePointer};

    fn release(keys: &Keys, version: &str, manifest: u8) -> Release {
        let data = ReleaseData {
            schema: "haps.release.v1".into(),
            package: PackageSpec {
                name: "hello".into(),
                version: version.parse().unwrap(),
                target: target().into(),
                description: "Friendly package".into(),
                commands: BTreeMap::new(),
                dependencies: vec![],
                source: None,
                app: None,
                desktop: None,
            },
            manifest: hashtree_core::Cid {
                hash: [manifest; 32],
                key: None,
            }
            .to_string(),
        };
        Release::verify(sign_release(keys, &data).unwrap()).unwrap()
    }

    fn head(keys: &Keys, release: &Release, at: u64) -> Event {
        let head = PackageHead {
            schema: "haps.package-head.v1".into(),
            description: "Friendly package".into(),
            payload: "https://packages.example.org".into(),
            card: release.data.manifest.clone(),
            releases: vec![ReleasePointer {
                id: release.event.id.to_hex(),
                cid: release.data.manifest.clone(),
                version: release.data.package.version.clone(),
                target: release.data.package.target.clone(),
            }],
        };
        EventBuilder::new(SOFTWARE_KIND, serde_json::to_string(&head).unwrap())
            .tags([
                Tag::identifier("hello"),
                Tag::parse(["name", "hello"]).unwrap(),
                Tag::hashtag("haps"),
                Tag::parse(["haps_head", "1"]).unwrap(),
            ])
            .custom_created_at(Timestamp::from(at))
            .sign_with_keys(keys)
            .unwrap()
    }

    fn relations(keys: &Keys, kind: Kind, people: &[&Keys], at: u64) -> Event {
        EventBuilder::new(kind, "")
            .tags(people.iter().map(|key| Tag::public_key(key.public_key())))
            .custom_created_at(Timestamp::from(at))
            .sign_with_keys(keys)
            .unwrap()
    }

    #[test]
    fn snapshots_start_empty_then_add_verified_release_and_remove_revoked_trust() {
        let publisher = Keys::generate();
        let me = Keys::generate();
        let release = release(&publisher, "1.0.0", 1);
        let at = Timestamp::now().as_secs() - 3;
        let mut results = Results::default();
        let mut trust = Trust::new(me.public_key().to_hex());
        let mut previous = None;
        let mut output = Vec::new();
        let config = Config::default();
        emit(
            &results,
            "hello friendly",
            &trust,
            &config,
            &mut previous,
            &mut output,
        )
        .unwrap();
        results.ingest(
            &[head(&publisher, &release, at), release.event.clone()],
            &mut trust,
        );
        emit(
            &results,
            "hello friendly",
            &trust,
            &config,
            &mut previous,
            &mut output,
        )
        .unwrap();
        assert_eq!(String::from_utf8(output.clone()).unwrap(), "[]\n");
        results.ingest(
            &[relations(&me, Kind::ContactList, &[&publisher], at)],
            &mut trust,
        );
        emit(
            &results,
            "hello friendly",
            &trust,
            &config,
            &mut previous,
            &mut output,
        )
        .unwrap();
        results.ingest(
            &[relations(&me, Kind::MuteList, &[&publisher], at + 1)],
            &mut trust,
        );
        emit(
            &results,
            "hello friendly",
            &trust,
            &config,
            &mut previous,
            &mut output,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        let snapshots: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(snapshots.len(), 3);
        assert_eq!(snapshots[0], serde_json::json!([]));
        assert_eq!(snapshots[1][0]["release_id"], release.event.id.to_hex());
        assert_eq!(snapshots[2], serde_json::json!([]));
    }

    #[test]
    fn newer_head_replaces_same_version_id_without_reviving_slow_catalog_or_head() {
        let publisher = Keys::generate();
        let old = release(&publisher, "1.0.0", 1);
        let new = release(&publisher, "1.0.0", 2);
        let at = Timestamp::now().as_secs() - 3;
        let old_head = head(&publisher, &old, at);
        let new_head = head(&publisher, &new, at + 1);
        let mut trust = Trust::new(publisher.public_key().to_hex());
        let mut results = Results::default();
        results.ingest(&[old_head.clone(), old.event.clone()], &mut trust);
        assert_eq!(
            results.visible("hello", &trust).unwrap()[0].event.id,
            old.event.id
        );
        results.ingest(&[new_head], &mut trust);
        assert!(
            results.visible("hello", &trust).unwrap().is_empty(),
            "a head alone must not display an unverified pointer"
        );
        results.catalogs.insert(
            (publisher.public_key().to_hex(), "legacy".into()),
            vec![old.clone()],
        );
        results.ingest(&[old_head, new.event.clone()], &mut trust);
        let visible = results.visible("hello", &trust).unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].event.id, new.event.id);
    }

    #[test]
    fn withdrawing_the_only_vouch_removes_a_release_from_the_next_snapshot() {
        let publisher = Keys::generate();
        let friend = Keys::generate();
        let me = Keys::generate();
        let release = release(&publisher, "2.0.0", 1);
        let at = Timestamp::now().as_secs() - 3;
        let mut trust = Trust::new(me.public_key().to_hex());
        let mut results = Results::default();
        results.ingest(
            &[
                head(&publisher, &release, at),
                release.event.clone(),
                relations(&me, Kind::ContactList, &[&friend], at),
                attest_at(&friend, release.event.id, true, "Checked".into(), at * 1000).unwrap(),
            ],
            &mut trust,
        );
        assert_eq!(results.visible("hello", &trust).unwrap().len(), 1);
        results.ingest(
            &[attest_at(
                &friend,
                release.event.id,
                false,
                "Withdrawn".into(),
                (at + 1) * 1000,
            )
            .unwrap()],
            &mut trust,
        );
        assert!(results.visible("hello", &trust).unwrap().is_empty());
    }

    #[test]
    fn invalid_signed_pointer_or_forged_event_cannot_become_a_result() {
        let publisher = Keys::generate();
        let release = release(&publisher, "1.0.0", 1);
        let at = Timestamp::now().as_secs() - 3;
        let mut forged = serde_json::to_value(&release.event).unwrap();
        forged["content"] = "forged".into();
        let mut trust = Trust::new(publisher.public_key().to_hex());
        let mut results = Results::default();
        results.ingest(
            &[
                head(&publisher, &release, at),
                serde_json::from_value(forged).unwrap(),
            ],
            &mut trust,
        );
        assert!(results.visible("hello", &trust).unwrap().is_empty());
        let mut wrong = head(&publisher, &release, at);
        let mut data: PackageHead = serde_json::from_str(&wrong.content).unwrap();
        data.releases[0].version = "9.0.0".parse().unwrap();
        wrong = EventBuilder::new(SOFTWARE_KIND, serde_json::to_string(&data).unwrap())
            .tags(wrong.tags.iter().cloned())
            .custom_created_at(Timestamp::from(at + 1))
            .sign_with_keys(&publisher)
            .unwrap();
        results.ingest(&[wrong, release.event], &mut trust);
        assert!(results.visible("hello", &trust).is_err());
    }

    #[test]
    fn stream_requires_json_and_plain_json_remains_supported() {
        assert!(Cli::try_parse_from(["haps", "search", "--stream", ""]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["haps", "search", "--json", "--stream", ""])
                .unwrap()
                .command,
            Command::Search {
                json: true,
                stream: true,
                ..
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["haps", "search", "--json", ""])
                .unwrap()
                .command,
            Command::Search {
                json: true,
                stream: false,
                ..
            }
        ));
    }
}
