//! A catalog is a selected index of original Nostr events, advertised by its owner.
use crate::{
    discovery::{Announcement, Discovery},
    model::*,
};
use anyhow::{Context, Result, ensure};
use hashtree_core::Cid;
use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub const INDEX_KIND: Kind = Kind::Custom(hashtree_nostr::HASHTREE_ROOT_KIND as u16);
pub const INDEX_LABEL: &str = "nostr-event-index";
pub struct IndexAnnouncement {
    pub event: Event,
    pub name: String,
    pub location: String,
}
impl IndexAnnouncement {
    pub fn verify(event: Event) -> Result<Self> {
        let parsed = hashtree_nostr::parse_verified_hashtree_root_event(&event)?
            .context("not a Hashtree root announcement")?;
        tag_value(&event, "d")?;
        ensure!(
            parsed.encrypted_key.is_none() && parsed.self_encrypted_key.is_none(),
            "index must have a public root"
        );
        let name = if parsed.tree_name == INDEX_LABEL {
            INDEX_LABEL.to_string()
        } else {
            parsed
                .tree_name
                .strip_prefix("nostr-event-index/")
                .context("not a Nostr event index announcement")?
                .to_string()
        };
        safe_name(&name)?;
        let location = root_url(&parsed.root_cid)?;
        Ok(Self {
            event,
            name,
            location,
        })
    }
    pub fn sign(keys: &Keys, name: &str, location: &str) -> Result<Event> {
        safe_name(name)?;
        let root = decode_root(location)?;
        // Keep a curated collection distinct from a daemon's general archive.
        let tree = format!("{INDEX_LABEL}/{name}");
        // Same public-root wire format as htree's ordinary Nostr event indexes.
        let mut tags = vec![
            Tag::identifier(tree),
            Tag::parse(["l", "hashtree"])?,
            Tag::parse(["l", INDEX_LABEL])?,
            Tag::parse(["hash", &hex::encode(root.hash)])?,
        ];
        if let Some(key) = root.key {
            tags.push(Tag::parse(["key", &hex::encode(key)])?);
        }
        let event = EventBuilder::new(INDEX_KIND, "")
            .tags(tags)
            .sign_with_keys(keys)?;
        Self::verify(event.clone())?;
        Ok(event)
    }
}

pub fn decode_root(location: &str) -> Result<Cid> {
    let hash = location.strip_prefix("htree://").unwrap_or(location);
    let decoded = hashtree_core::nhash_decode(hash)?;
    Ok(Cid {
        hash: decoded.hash,
        key: decoded.decrypt_key,
    })
}

pub fn root_url(root: &Cid) -> Result<String> {
    Ok(format!(
        "htree://{}",
        hashtree_core::nhash_encode_full(&hashtree_core::NHashData {
            hash: root.hash,
            decrypt_key: root.key
        })?
    ))
}

/// Import selected events into an isolated native htree store and push its DAG.
/// No directory wrapper, Haps manifest, or private browsing records are included.
pub fn upload_index(events: &[Event]) -> Result<String> {
    ensure!(
        !events.is_empty(),
        "catalog is empty; add a package before publishing"
    );
    let helper = crate::helpers::find("htree").context("Publishing needs htree. Install the Haps bundle or run `cargo install hashtree-cli --locked`.")?;
    let staging = tempfile::tempdir()?;
    let input = staging.path().join("events.json");
    std::fs::write(&input, serde_json::to_vec(events)?)?;
    let data = staging.path().join("data");
    let output = std::process::Command::new(&helper)
        .arg("--data-dir")
        .arg(&data)
        .args(["nostr-index", "import", "--events"])
        .arg(input)
        .output()?;
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    ensure!(
        output.status.success(),
        "Hashtree event index import failed"
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let hash = report["root"]
        .as_str()
        .context("htree did not return an index root")?;
    let root = decode_root(hash)?;
    let output = std::process::Command::new(helper)
        .arg("--data-dir")
        .arg(data)
        .args(["push", hash])
        .output()?;
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    ensure!(
        output.status.success(),
        "Hashtree content upload failed; catalog was not announced"
    );
    root_url(&root)
}

pub fn directory(home: &Path, name: &str) -> Result<PathBuf> {
    safe_name(name)?;
    Ok(home.join("catalogs").join(name))
}

/// Preserve current events when several user operations occur within one second.
pub fn advance(keys: &Keys, event: Event, previous: &[Event]) -> Result<Event> {
    let identifier = tag_value(&event, "d")?;
    let latest = previous
        .iter()
        .filter(|old| {
            old.pubkey == event.pubkey
                && old.kind == event.kind
                && tag_value(old, "d").ok() == Some(identifier)
        })
        .map(|old| old.created_at)
        .max();
    if let Some(latest) = latest
        && latest >= event.created_at
    {
        return Ok(EventBuilder::new(event.kind, event.content)
            .tags(event.tags)
            .custom_created_at(Timestamp::from(
                latest
                    .as_secs()
                    .checked_add(1)
                    .context("timestamp overflow")?,
            ))
            .sign_with_keys(keys)?);
    }
    Ok(event)
}

pub async fn add_package(home: &Path, name: &str, package: &str) -> Result<usize> {
    let source = Discovery::open(home)?;
    let cached: BTreeMap<_, _> = source
        .events(vec![nostr::Filter::new().kind(APP_KIND)])
        .await?
        .into_iter()
        .map(|event| (event.id.to_hex(), event))
        .collect();
    let matches: Vec<_> = source
        .announcements(None)
        .await?
        .into_iter()
        .filter(|a| package == a.name || package == format!("{}/{}", a.author(), a.name))
        .collect();
    ensure!(!matches.is_empty(), "package announcement unavailable");
    ensure!(
        matches
            .iter()
            .map(Announcement::author)
            .collect::<BTreeSet<_>>()
            .len()
            == 1,
        "package is ambiguous; use publisher/name"
    );
    let mut events = Vec::new();
    for announcement in matches {
        if let Some(ref head) = announcement.head {
            let repo = crate::repository::Repository::open(&head.payload, &home.join("cache"))?;
            for pointer in &head.releases {
                let event = match cached.get(&pointer.id) {
                    Some(event) => event.clone(),
                    None => repo.json(&pointer.cid).await?,
                };
                let release = head.verify_release(
                    pointer,
                    event,
                    &announcement.author(),
                    &announcement.name,
                )?;
                events.push(release.event);
            }
        }
        events.push(announcement.event);
    }
    drop(source);
    Discovery::open(&directory(home, name)?)?
        .ingest(events)
        .await
}

/// Confirm upload before using the immutable root in signed discovery events.
pub fn upload(path: &Path, legacy_name: Option<&str>) -> Result<String> {
    let helper = crate::helpers::find("htree").context("Publishing needs htree. Install the Haps bundle or run `cargo install hashtree-cli --locked`.")?;
    let mut command = std::process::Command::new(&helper);
    command.arg("add").arg(path);
    if let Some(name) = legacy_name {
        ensure!(
            !name.is_empty() && !name.starts_with('-'),
            "invalid catalog name"
        );
        command.args(["--publish", name]);
    } else {
        // Unnamed `htree add` treats an automatic push failure as a warning.
        // Store locally first, then use the explicit push command's exit status.
        command.arg("--local");
    }
    let output = command.output()?;
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    ensure!(
        output.status.success(),
        "Hashtree catalog publication failed"
    );
    let text = String::from_utf8(output.stdout)?;
    let hash = text
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("url: ")
                .filter(|value| value.starts_with("nhash"))
        })
        .context("htree did not return an immutable nhash; update hashtree-cli")?;
    hashtree_core::nhash_decode(hash)?;
    if legacy_name.is_none() {
        let output = std::process::Command::new(helper)
            .args(["push", hash])
            .output()?;
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        ensure!(
            output.status.success(),
            "Hashtree content upload failed; catalog was not announced"
        );
    }
    Ok(format!("htree://{hash}"))
}

pub async fn package_events(
    home: &Path,
    path: &Path,
    keys: &Keys,
    legacy_name: Option<&str>,
) -> Result<Vec<Event>> {
    let repo = crate::repository::Repository::local(path.to_path_buf())?;
    let snapshot = repo.catalog(&keys.public_key().to_hex()).await?;
    let releases = repo.releases(&snapshot).await?;
    let location = upload(path, legacy_name)?;
    let previous = Discovery::open(home)?
        .events(vec![nostr::Filter::new()])
        .await?;
    let mut events: Vec<_> = releases.iter().map(|r| r.event.clone()).collect();
    let names: BTreeSet<_> = releases
        .iter()
        .filter(|r| r.event.pubkey == keys.public_key())
        .map(|r| r.data.package.name.as_str())
        .collect();
    for name in names {
        let event = Announcement::sign_direct(keys, &repo, &snapshot, name, &location).await?;
        events.push(advance(keys, event, &previous)?);
    }
    Ok(events)
}
