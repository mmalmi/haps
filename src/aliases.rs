//! Adapter to the shared hashtree public aliases file. The format and path
//! belong to hashtree-config and are also consumed by git-remote-htree.
use crate::model::{MAX_METADATA, atomic_write, lock, safe_name};
use anyhow::{Result, ensure};
use nostr::{PublicKey, nips::nip19::ToBech32};
use std::{collections::BTreeMap, fs};

fn content() -> Result<String> {
    let path = hashtree_config::get_aliases_path();
    if !path.exists() {
        return Ok(String::new());
    }
    ensure!(
        fs::metadata(&path)?.len() <= MAX_METADATA as u64,
        "shared aliases file is too large"
    );
    Ok(fs::read_to_string(path)?)
}

pub fn read() -> Result<BTreeMap<String, String>> {
    let mut aliases = BTreeMap::new();
    for entry in hashtree_config::parse_keys_file(&content()?) {
        if let Some(name) = entry.alias {
            // Never interpret public aliases as signing keys.
            if let Ok(key) = PublicKey::parse(&entry.secret)
                && let Some(old) = aliases.insert(name, key.to_hex())
            {
                ensure!(
                    old == key.to_hex(),
                    "conflicting names in shared hashtree aliases"
                );
            }
        }
    }
    Ok(aliases)
}

pub fn add(name: &str, key: &str) -> Result<()> {
    safe_name(name)?;
    ensure!(
        PublicKey::parse(name).is_err(),
        "alias must not look like a public key"
    );
    let key = PublicKey::parse(key)?;
    let path = hashtree_config::get_aliases_path();
    let _guard = lock(&path.with_extension("lock"))?;
    ensure!(
        !hashtree_config::parse_keys_file(&content()?)
            .iter()
            .any(|e| e.alias.as_deref() == Some(name)),
        "alias already exists; remove it explicitly before assigning another publisher"
    );
    let text = format!("{}\n{} {name}\n", content()?.trim_end(), key.to_bech32()?);
    atomic_write(&path, text.as_bytes())
}

pub fn remove(name: &str) -> Result<()> {
    let path = hashtree_config::get_aliases_path();
    let _guard = lock(&path.with_extension("lock"))?;
    let old = content()?;
    let mut removed = false;
    let lines: Vec<_> = old
        .lines()
        .filter(|line| {
            let matches = hashtree_config::parse_keys_file(line)
                .iter()
                .any(|e| e.alias.as_deref() == Some(name));
            removed |= matches;
            !matches
        })
        .collect();
    ensure!(removed, "alias not found");
    atomic_write(&path, format!("{}\n", lines.join("\n")).as_bytes())
}
