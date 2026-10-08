//! Explicit audit fixtures for tests of unrelated installation/transport behavior.
#![allow(dead_code)]
use nostr::{Event, EventId, Keys};
use std::{fs, path::Path};

pub fn approval(keys: &Keys, release: EventId) -> anyhow::Result<Event> {
    haps::trust::attest_audit_at(
        keys,
        release,
        "Reviewed test fixture bytes".into(),
        &haps::audit::Evidence::manual("Test fixture; reproduction not claimed".into())?,
        nostr::Timestamp::now().as_secs() * 1000,
    )
}

pub fn record(home: &Path, keys: &Keys, release: EventId) -> anyhow::Result<()> {
    fs::create_dir_all(home)?;
    let path = home.join("social.json");
    let mut events: Vec<Event> = if path.exists() {
        haps::model::read_json(&path)?
    } else {
        vec![]
    };
    events.push(approval(keys, release)?);
    fs::write(path, serde_json::to_vec(&events)?)?;
    Ok(())
}

pub fn local(home: &Path, release: EventId) -> anyhow::Result<()> {
    let path = home.join("identity.key");
    record(home, &haps::install::load_keys(&path)?, release)
}
