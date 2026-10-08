use crate::model::{APP_KIND, Release, tag_value};
use anyhow::{Context, Result, ensure};
use nostr::{Event, EventId, Keys, Kind};
use nostr_identity::{
    FACT_SNAPSHOT_KIND, build_fact_snapshot_event_with_created_at_ms, compare_fact_snapshots, fact,
    parse_fact_snapshot_event,
};
use nostr_social_graph::SocialGraph;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

// Match Iris feed/recommendation visibility; the algorithm lives in the shared graph.
const SOCIAL_GRAPH_OVERMUTE_THRESHOLD: f64 = 3.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attestation {
    pub schema: String,
    pub release: String,
    pub approved: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub warning: bool,
    pub note: String,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// Write a fact snapshot about the exact signed release, using its event hash as subject.
pub fn attest(keys: &Keys, release: EventId, approved: bool, note: String) -> Result<Event> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    attest_at(keys, release, approved, note, u64::try_from(now)?)
}

pub fn attest_at(
    keys: &Keys,
    release: EventId,
    approved: bool,
    note: String,
    at_ms: u64,
) -> Result<Event> {
    claim_at(keys, release, approved, false, note, at_ms)
}

/// Explicit audit approval, replacing the signer's previous claim for this release.
pub fn attest_audit_at(
    keys: &Keys,
    release: EventId,
    note: String,
    evidence: &crate::audit::Evidence,
    at_ms: u64,
) -> Result<Event> {
    evidence.validate()?;
    ensure!(
        !note.trim().is_empty() && note.len() <= 4096,
        "describe the audit with --note"
    );
    let audit = serde_json::to_string(evidence)?;
    build_fact_snapshot_event_with_created_at_ms(
        keys,
        release.to_hex(),
        [
            fact("type", &["haps_release_attestation"]),
            fact("schema", &["1"]),
            fact("approved", &["true"]),
            fact("warning", &["false"]),
            fact("note", &[&note]),
            fact("audit", &[&audit]),
        ],
        [],
        at_ms / 1000,
        at_ms,
    )
}

pub fn parse_audit(event: &Event) -> Result<crate::audit::Evidence> {
    let claim = parse_attestation(event)?;
    ensure!(
        claim.approved && !claim.warning,
        "claim does not approve an audit"
    );
    let snapshot = parse_fact_snapshot_event(event)?;
    let facts: Vec<_> = snapshot
        .facts
        .iter()
        .filter(|f| f.predicate == "audit")
        .collect();
    ensure!(
        facts.len() == 1 && facts[0].values.len() == 1,
        "claim needs one audit record"
    );
    let evidence: crate::audit::Evidence = serde_json::from_str(&facts[0].values[0])?;
    evidence.validate()?;
    Ok(evidence)
}

pub fn warn(keys: &Keys, release: EventId, active: bool, note: String) -> Result<Event> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    warn_at(keys, release, active, note, u64::try_from(now)?)
}

pub fn warn_at(
    keys: &Keys,
    release: EventId,
    active: bool,
    note: String,
    at_ms: u64,
) -> Result<Event> {
    claim_at(keys, release, false, active, note, at_ms)
}

fn claim_at(
    keys: &Keys,
    release: EventId,
    approved: bool,
    warning: bool,
    note: String,
    at_ms: u64,
) -> Result<Event> {
    ensure!(note.len() <= 4096, "attestation note is too long");
    build_fact_snapshot_event_with_created_at_ms(
        keys,
        release.to_hex(),
        [
            fact("type", &["haps_release_attestation"]),
            fact("schema", &["1"]),
            fact("approved", &[if approved { "true" } else { "false" }]),
            fact("warning", &[if warning { "true" } else { "false" }]),
            fact("note", &[&note]),
        ],
        [],
        at_ms / 1000,
        at_ms,
    )
}

/// Decode the Haps profile; other kinds of facts never authorize installation.
pub fn parse_attestation(event: &Event) -> Result<Attestation> {
    event.verify()?;
    let claim = if event.kind == Kind::from(FACT_SNAPSHOT_KIND) {
        let snapshot = parse_fact_snapshot_event(event)?;
        let scalar = |predicate: &str| -> Result<&str> {
            let matches: Vec<_> = snapshot
                .facts
                .iter()
                .filter(|f| f.predicate == predicate)
                .collect();
            ensure!(
                matches.len() == 1 && matches[0].values.len() == 1,
                "attestation needs one {predicate}"
            );
            Ok(matches[0].values[0].as_str())
        };
        ensure!(
            scalar("type")? == "haps_release_attestation" && scalar("schema")? == "1",
            "unsupported attestation profile"
        );
        let approved = match scalar("approved")? {
            "true" => true,
            "false" => false,
            _ => anyhow::bail!("attestation approved must be true or false"),
        };
        let warning = if snapshot.facts.iter().any(|f| f.predicate == "warning") {
            match scalar("warning")? {
                "true" => true,
                "false" => false,
                _ => anyhow::bail!("attestation warning must be true or false"),
            }
        } else {
            false
        };
        ensure!(
            !(approved && warning),
            "a claim cannot approve and warn simultaneously"
        );
        Attestation {
            schema: "haps.attestation.v1".into(),
            release: snapshot.subject.clone(),
            approved,
            warning,
            note: scalar("note")?.into(),
        }
    } else {
        ensure!(
            event.kind == APP_KIND,
            "expected a Haps release attestation"
        );
        let claim: Attestation = serde_json::from_str(&event.content)?;
        ensure!(!claim.warning, "warnings require a fact snapshot");
        ensure!(
            tag_value(event, "d")? == format!("haps/attestation/{}", claim.release),
            "attestation target mismatch"
        );
        claim
    };
    ensure!(
        claim.schema == "haps.attestation.v1" && claim.note.len() <= 4096,
        "invalid attestation"
    );
    ensure!(
        EventId::from_hex(&claim.release)?.to_hex() == claim.release,
        "attestation release must be a canonical event hash"
    );
    Ok(claim)
}

pub fn attestation_time_ms(event: &Event) -> Result<u64> {
    let seconds_ms = event
        .created_at
        .as_secs()
        .checked_mul(1000)
        .context("attestation timestamp overflow")?;
    if event.kind == Kind::from(FACT_SNAPSHOT_KIND) {
        Ok(parse_fact_snapshot_event(event)?
            .created_at_ms
            .unwrap_or(seconds_ms))
    } else {
        Ok(seconds_ms)
    }
}

fn newer_attestation(event: &Event, old: &Event) -> Result<bool> {
    if event.kind == Kind::from(FACT_SNAPSHOT_KIND) && old.kind == event.kind {
        return Ok(compare_fact_snapshots(
            &parse_fact_snapshot_event(event)?,
            &parse_fact_snapshot_event(old)?,
        )
        .is_gt());
    }
    if event.kind == APP_KIND && old.kind == APP_KIND {
        // Preserve the legacy replaceable-event tie break when reading old stores.
        return Ok(event.created_at > old.created_at
            || (event.created_at == old.created_at && event.id < old.id));
    }
    Ok((
        attestation_time_ms(event)?,
        event.kind == Kind::from(FACT_SNAPSHOT_KIND),
    ) > (
        attestation_time_ms(old)?,
        old.kind == Kind::from(FACT_SNAPSHOT_KIND),
    ))
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
            let attestation = parse_attestation(&event)?;
            format!("haps/attestation/{}", attestation.release)
        };
        let key = format!("{}/{}", event.pubkey.to_hex(), identifier);
        if let Some(old) = self.events.get(&key) {
            let newer = if event.kind == Kind::ContactList || event.kind == Kind::MuteList {
                event.created_at > old.created_at
                    || (event.created_at == old.created_at && event.id < old.id)
            } else {
                newer_attestation(&event, old)?
            };
            if !newer {
                return Ok(());
            }
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
            // Reconstruct all verified latest records before applying visibility.
            // handle_event's admission filter depends on a partially built graph
            // and can otherwise discard lists based on public-key iteration order.
            for key in event.tags.public_keys() {
                if event.kind == Kind::ContactList {
                    self.graph.add_positive_relation(
                        &event.pubkey.to_hex(),
                        &key.to_hex(),
                        event.created_at.as_secs(),
                    )?;
                } else {
                    self.graph.add_negative_relation(
                        &event.pubkey.to_hex(),
                        &key.to_hex(),
                        event.created_at.as_secs(),
                    )?;
                }
            }
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
    /// Bounded social discovery; known nearer authors take priority.
    pub fn discovery_authors(&self, distance: u32) -> Vec<nostr::PublicKey> {
        let mut authors: Vec<_> = self
            .reachable
            .iter()
            .filter(|key| self.relevant_signer(key))
            .filter_map(|key| Some((self.distance(key)?, nostr::PublicKey::parse(key).ok()?)))
            .filter(|(d, _)| *d <= distance)
            .collect();
        authors.sort();
        authors.into_iter().take(256).map(|(_, key)| key).collect()
    }
    pub fn muted(&self, author: &str) -> bool {
        self.graph
            .get_muted_by_user(&self.root)
            .iter()
            .any(|p| p == author)
    }
    pub fn overmuted(&self, author: &str) -> bool {
        self.graph
            .is_overmuted(author, SOCIAL_GRAPH_OVERMUTE_THRESHOLD)
    }
    /// Shared default for discovery and installation. Curation alone is not a vouch.
    pub fn socially_trusted(&self, release: &Release) -> bool {
        !self.muted(&release.author())
            && (self.relevant_signer(&release.author()) || !self.attestations(release).is_empty())
    }
    /// Unmuted direct connections that follow this publisher, as in Iris Contacts.
    pub fn followed_by_friends(&self, author: &str) -> Vec<String> {
        let mut friends: Vec<_> = self
            .graph
            .get_followed_by_user(&self.root)
            .into_iter()
            .filter(|key| {
                key != &self.root
                    && self.relevant_signer(key)
                    && self.graph.is_following(key, author)
            })
            .collect();
        friends.sort();
        friends
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
        for event in self
            .events
            .values()
            .filter(|e| e.kind == APP_KIND || e.kind == Kind::from(FACT_SNAPSHOT_KIND))
        {
            let author = event.pubkey.to_hex();
            if author == release.author() || !self.relevant_signer(&author) {
                continue;
            }
            if let Ok(a) = parse_attestation(event)
                && a.release == release.event.id.to_hex()
                && a.approved
            {
                attestations.push(event);
            }
        }
        attestations
    }
    fn relevant_signer(&self, author: &str) -> bool {
        !self.muted(author) && !self.overmuted(author) && self.distance(author).is_some()
    }
    /// Explicit audits only; publisher self-approvals count only for that user's
    /// own local installation, never as an independent audit for other readers.
    pub fn audits(&self, release: &Release) -> Vec<&Event> {
        self.events
            .values()
            .filter(|event| {
                let author = event.pubkey.to_hex();
                (author != release.author() || author == self.root)
                    && self.relevant_signer(&author)
                    && parse_attestation(event)
                        .is_ok_and(|a| a.release == release.event.id.to_hex())
                    && parse_audit(event).is_ok()
            })
            .collect()
    }

    /// Default installation policy. Trust overrides and a zero legacy threshold
    /// do not remove the audit requirement; a CLI audit bypass is separate.
    pub fn authorize_install(
        &self,
        release: &Release,
        allow_untrusted: bool,
        minimum_audits: usize,
        allow_warnings: bool,
    ) -> Result<()> {
        self.authorize_with_policy(release, allow_untrusted, 0, allow_warnings)?;
        let minimum_audits = minimum_audits.max(1);
        let count = self.audits(release).len();
        ensure!(
            count >= minimum_audits,
            "release requires {minimum_audits} audit(s) from your social graph; found {count}. Use Audit and install, record an explicit audit with haps attest --audited --provenance TEXT --note TEXT, or bypass for this operation with --allow-unaudited"
        );
        Ok(())
    }
    /// Current warnings from reachable, non-overmuted graph members.
    /// A followed publisher may warn about its own release (a recall).
    pub fn warnings(&self, release: &Release) -> Vec<&Event> {
        self.events
            .values()
            .filter(|event| {
                let author = event.pubkey.to_hex();
                self.relevant_signer(&author)
                    && parse_attestation(event).is_ok_and(|claim| {
                        claim.warning && claim.release == release.event.id.to_hex()
                    })
            })
            .collect()
    }
    pub fn authorize(
        &self,
        release: &Release,
        allow_untrusted: bool,
        minimum_attestations: usize,
    ) -> Result<()> {
        self.authorize_with_policy(release, allow_untrusted, minimum_attestations, false)
    }
    pub fn authorize_with_policy(
        &self,
        release: &Release,
        allow_untrusted: bool,
        minimum_attestations: usize,
        allow_warnings: bool,
    ) -> Result<()> {
        ensure!(!self.muted(&release.author()), "publisher is muted");
        let warnings = self.warnings(release);
        if !allow_warnings && !warnings.is_empty() {
            let findings: Vec<_> = warnings
                .iter()
                .map(|event| {
                    let claim = parse_attestation(event).expect("validated warning");
                    format!("{}: {}", event.pubkey.to_hex(), claim.note.escape_debug())
                })
                .collect();
            anyhow::bail!(
                "trusted release warnings: {}. Inspect with haps info; use --allow-warnings only to explicitly override for this operation",
                findings.join("; ")
            );
        }
        let attestations = self.attesters(release).len();
        ensure!(
            attestations >= minimum_attestations,
            "release requires {minimum_attestations} attestations from non-overmuted members of your social graph; found {attestations}"
        );
        ensure!(
            allow_untrusted || self.socially_trusted(release),
            "release is not authored or vouched for by your social graph; inspect the public key and use npub/package explicitly, or --allow-untrusted"
        );
        Ok(())
    }
}
