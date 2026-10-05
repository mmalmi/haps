//! NIP-22 comments. Package threads use stable addresses; release threads use
//! exact signed event IDs, so discussion never silently moves to newer bytes.
use crate::model::{APP_KIND, Release, safe_name, tag_value, verify_event};
use anyhow::{Context, Result, ensure};
use nostr::nips::nip22::{CommentTarget, extract_parent, extract_root};
use nostr::{Event, EventBuilder, EventId, Keys, Kind, Tag};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "scope", rename_all = "lowercase")]
pub enum Scope {
    Package {
        publisher: String,
        name: String,
    },
    Release {
        publisher: String,
        release_id: String,
    },
}

impl Scope {
    pub fn package(release: &Release) -> Self {
        Self::Package {
            publisher: release.author(),
            name: release.data.package.name.clone(),
        }
    }
    pub fn release(release: &Release) -> Self {
        Self::Release {
            publisher: release.author(),
            release_id: release.event.id.to_hex(),
        }
    }
}

fn scope_of_target(target: &CommentTarget<'_>) -> Result<Scope> {
    match target {
        CommentTarget::Coordinate { address, .. } => {
            ensure!(
                address.kind == APP_KIND,
                "comment targets another application"
            );
            let name = address
                .identifier
                .strip_prefix("haps/package/")
                .context("not a Haps package thread")?;
            safe_name(name)?;
            Ok(Scope::Package {
                publisher: address.public_key.to_hex(),
                name: name.into(),
            })
        }
        CommentTarget::Event {
            id,
            pubkey_hint: Some(author),
            kind: Some(kind),
            ..
        } if *kind == APP_KIND => Ok(Scope::Release {
            publisher: author.to_hex(),
            release_id: id.to_hex(),
        }),
        _ => anyhow::bail!("invalid Haps comment scope"),
    }
}

pub fn verify(event: &Event) -> Result<Scope> {
    event.verify()?;
    ensure!(event.kind == Kind::Comment, "expected a NIP-22 comment");
    ensure!(
        !event.content.trim().is_empty() && event.content.len() <= 16_384,
        "comment must contain 1–16384 bytes of text"
    );
    ensure!(event.tags.len() <= 64, "too many comment tags");
    ensure!(
        event.created_at.as_secs() <= nostr::Timestamp::now().as_secs() + 300,
        "comment is too far in the future"
    );
    for name in ["K", "P", "k"] {
        ensure!(
            event
                .tags
                .iter()
                .filter(|t| t.as_slice().first().is_some_and(|v| v == name))
                .count()
                == 1,
            "ambiguous comment {name} tag"
        );
    }
    ensure!(
        event
            .tags
            .iter()
            .filter(|t| t
                .as_slice()
                .first()
                .is_some_and(|v| ["A", "E", "I"].contains(&v.as_str())))
            .count()
            == 1,
        "ambiguous comment root"
    );
    let root = scope_of_target(&extract_root(event).context("comment root is missing")?)?;
    // NIP-22 addressable parents include both `a` and a concrete `e`. The
    // nostr helper prefers `e`; validate the stable address first in that case.
    let address_parent = event.tags.iter().find_map(|tag| {
        let values = tag.as_slice();
        (values.first().is_some_and(|v| v == "a"))
            .then(|| values.get(1))
            .flatten()
    });
    if let Some(address) = address_parent {
        let coordinate = nostr::nips::nip01::Coordinate::parse(address)?;
        let parent = CommentTarget::coordinate(std::borrow::Cow::Owned(coordinate), None);
        ensure!(
            scope_of_target(&parent)? == root,
            "package comment parent differs from root"
        );
        ensure!(
            tag_value(event, "k")? == "30078",
            "invalid package comment parent kind"
        );
        return Ok(root);
    }
    let parent = extract_parent(event).context("comment parent is missing")?;
    match &parent {
        CommentTarget::Event {
            kind: Some(Kind::Comment),
            pubkey_hint: Some(_),
            ..
        } => {}
        _ => ensure!(
            scope_of_target(&parent)? == root,
            "top-level comment parent differs from root"
        ),
    }
    Ok(root)
}

pub fn create(
    keys: &Keys,
    root: &Event,
    release_scope: bool,
    parent: Option<&Event>,
    text: String,
) -> Result<Event> {
    verify_event(root)?;
    let root_target = if release_scope {
        Release::verify(root.clone())?;
        CommentTarget::event(root.id, root.kind, Some(root.pubkey), None)
    } else {
        let card: serde_json::Value = serde_json::from_str(&root.content)?;
        ensure!(card["schema"] == "haps.package.v1", "expected package card");
        let name = card["name"].as_str().context("package name is missing")?;
        safe_name(name)?;
        ensure!(
            tag_value(root, "d")? == format!("haps/package/{name}"),
            "package card identity mismatch"
        );
        CommentTarget::from(root)
    };
    let expected_scope = scope_of_target(&root_target)?;
    let parent_target = if let Some(parent) = parent {
        ensure!(
            verify(parent)? == expected_scope,
            "reply belongs to a different thread"
        );
        CommentTarget::event(parent.id, Kind::Comment, Some(parent.pubkey), None)
    } else if release_scope {
        CommentTarget::event(root.id, root.kind, Some(root.pubkey), None)
    } else {
        CommentTarget::from(root)
    };
    let mut builder = EventBuilder::comment(text, parent_target, Some(root_target));
    if !release_scope && parent.is_none() {
        builder = builder.tags([Tag::event(root.id)]);
    }
    let event = builder.sign_with_keys(keys)?;
    verify(&event)?;
    Ok(event)
}

#[derive(Default)]
pub struct Comments {
    events: BTreeMap<EventId, Event>,
}

impl Comments {
    pub fn ingest(&mut self, event: Event) -> Result<()> {
        verify(&event)?;
        ensure!(
            self.events.len() < 10_000 || self.events.contains_key(&event.id),
            "comment limit exceeded"
        );
        self.events.insert(event.id, event);
        Ok(())
    }
    pub fn get(&self, id: &EventId) -> Option<&Event> {
        self.events.get(id)
    }
    pub fn events(&self) -> Vec<Event> {
        self.events.values().cloned().collect()
    }
    pub fn thread(&self, scope: &Scope) -> Vec<&Event> {
        let mut events: Vec<_> = self
            .events
            .values()
            .filter(|event| verify(event).as_ref().is_ok_and(|s| s == scope))
            .collect();
        events.sort_by_key(|e| (e.created_at, e.id));
        events
    }
}
