use crate::model::{APP_KIND, Release, sign, tag_value};
use anyhow::{Result, ensure};
use nostr::{Event, EventId, Keys, Kind};
use nostr_social_graph::{NostrEvent, SocialGraph};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attestation {
    pub schema: String,
    pub release: String,
    pub approved: bool,
    pub note: String,
}

pub fn attest(keys: &Keys, release: EventId, approved: bool, note: String) -> Result<Event> {
    ensure!(note.len() <= 4096, "attestation note is too long");
    sign(
        keys,
        &format!("haps/attestation/{release}"),
        &Attestation {
            schema: "haps.attestation.v1".into(),
            release: release.to_hex(),
            approved,
            note,
        },
    )
}

pub struct Trust {
    root: String,
    graph: SocialGraph,
    events: BTreeMap<String, Event>,
    reachable: BTreeSet<String>,
    starting_point: Option<String>,
}

impl Trust {
    pub fn new(root: String) -> Self {
        Self {
            graph: SocialGraph::new(&root),
            reachable: BTreeSet::from([root.clone()]),
            root,
            events: BTreeMap::new(),
            starting_point: None,
        }
    }
    pub fn ingest(&mut self, event: Event) -> Result<()> {
        event.verify()?;
        ensure!(
            event.created_at.as_secs() <= nostr::Timestamp::now().as_secs() + 300,
            "social event is too far in the future"
        );
        let identifier = if event.kind == Kind::ContactList || event.kind == Kind::MuteList {
            event.kind.as_u16().to_string()
        } else {
            ensure!(
                event.kind == APP_KIND,
                "expected signed follows, mutes, or a Haps attestation"
            );
            let attestation: Attestation = serde_json::from_str(&event.content)?;
            ensure!(
                attestation.schema == "haps.attestation.v1" && attestation.note.len() <= 4096,
                "invalid attestation"
            );
            EventId::from_hex(&attestation.release)?;
            let identifier = tag_value(&event, "d")?.to_string();
            ensure!(
                identifier == format!("haps/attestation/{}", attestation.release),
                "attestation target mismatch"
            );
            identifier
        };
        let key = format!("{}/{}", event.pubkey.to_hex(), identifier);
        if let Some(old) = self.events.get(&key)
            && (event.created_at < old.created_at
                || (event.created_at == old.created_at && event.id >= old.id))
        {
            return Ok(());
        }
        self.events.insert(key, event);
        self.rebuild()
    }
    fn rebuild(&mut self) -> Result<()> {
        // Rebuild from the latest signed records, independent of import order.
        self.graph = SocialGraph::new(&self.root);
        for event in self
            .events
            .values()
            .filter(|e| e.kind == Kind::ContactList || e.kind == Kind::MuteList)
        {
            self.graph.handle_event(
                &NostrEvent {
                    created_at: event.created_at.as_secs(),
                    content: event.content.clone(),
                    tags: event.tags.iter().map(|t| t.as_slice().to_vec()).collect(),
                    kind: event.kind.as_u16() as u32,
                    pubkey: event.pubkey.to_hex(),
                    id: event.id.to_hex(),
                    sig: event.sig.to_string(),
                },
                true,
                1.0,
            );
        }
        self.apply_starting_point()
    }
    /// An explicit local trust preference, not a fabricated signed follow event.
    pub fn set_starting_point(&mut self, key: Option<String>) -> Result<()> {
        self.starting_point = key;
        self.rebuild()
    }
    fn apply_starting_point(&mut self) -> Result<()> {
        if let Some(key) = &self.starting_point {
            self.graph.add_positive_relation(&self.root, key, 0)?;
        }
        self.graph.recalculate_follow_distances();
        self.reachable = self
            .graph
            .users_in_distance_order(None)
            .into_iter()
            .collect();
        Ok(())
    }
    pub fn events(&self) -> Vec<Event> {
        self.events.values().cloned().collect()
    }
    pub fn distance(&self, author: &str) -> Option<u32> {
        self.reachable
            .contains(author)
            .then(|| self.graph.get_follow_distance(author))
    }
    pub fn muted(&self, author: &str) -> bool {
        self.graph
            .get_muted_by_user(&self.root)
            .iter()
            .any(|p| p == author)
    }
    pub fn attesters(&self, release: &Release) -> Vec<String> {
        self.attestations(release)
            .iter()
            .map(|event| event.pubkey.to_hex())
            .collect()
    }
    /// Current positive attestations counted by the installation policy.
    pub fn attestations(&self, release: &Release) -> Vec<&Event> {
        let mut attestations = Vec::new();
        for event in self.events.values().filter(|e| e.kind == APP_KIND) {
            let author = event.pubkey.to_hex();
            if author == release.author()
                || self.muted(&author)
                || self.distance(&author).is_none_or(|d| d > 1)
            {
                continue;
            }
            if let Ok(a) = serde_json::from_str::<Attestation>(&event.content)
                && a.release == release.event.id.to_hex()
                && a.approved
            {
                attestations.push(event);
            }
        }
        attestations
    }
    pub fn authorize(
        &self,
        release: &Release,
        allow_untrusted: bool,
        minimum_attestations: usize,
    ) -> Result<()> {
        ensure!(!self.muted(&release.author()), "publisher is muted");
        let attestations = self.attesters(release).len();
        ensure!(
            attestations >= minimum_attestations,
            "release requires {minimum_attestations} attestations from you or keys you follow; found {attestations}"
        );
        ensure!(
            allow_untrusted
                || self.distance(&release.author()).is_some_and(|d| d <= 1)
                || (minimum_attestations > 0 && attestations >= minimum_attestations),
            "publisher is outside your direct follows; inspect the public key, follow them, require trusted attestations, or explicitly use --allow-untrusted"
        );
        Ok(())
    }
}
