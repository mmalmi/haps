use haps::trust::Trust;
use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp};
use nostr_social_graph::{SocialGraph, SocialGraphError};
use std::{fs, path::Path, process::Command};

fn cli(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_haps"))
        .arg("--home")
        .arg(home)
        .env("HTREE_CONFIG_DIR", home.join("hashtree"))
        .env_remove("HAPS_NO_DEFAULTS")
        .env_remove("NOSTR_RELAYS")
        .args(args)
        .output()
        .unwrap()
}

fn ok(home: &Path, args: &[&str]) {
    let output = cli(home, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn list(author: &Keys, kind: Kind, people: &[&Keys], at: u64) -> Event {
    EventBuilder::new(kind, "")
        .tags(people.iter().map(|key| Tag::public_key(key.public_key())))
        .custom_created_at(Timestamp::from(at))
        .sign_with_keys(author)
        .unwrap()
}

fn import(home: &Path, events: &[Event]) -> anyhow::Result<()> {
    let path = home.join("input.json");
    fs::write(&path, serde_json::to_vec(events)?)?;
    ok(home, &["import", path.to_str().unwrap()]);
    Ok(())
}

fn setup(home: &Path, me: &Keys, friend: &Keys) -> anyhow::Result<Vec<Event>> {
    // Later commands omit --no-defaults: reopening must respect this saved choice.
    ok(
        home,
        &[
            "--no-defaults",
            "identity",
            "use",
            &me.public_key().to_hex(),
        ],
    );
    let events = vec![
        list(me, Kind::ContactList, &[friend], 10),
        list(me, Kind::MuteList, &[friend], 10),
    ];
    import(home, &events)?;
    Ok(events)
}

fn conflicting_legacy_snapshot() -> Vec<u8> {
    // Binary v2: IDs 1 and 8 alias one pubkey, but their equally old follow
    // lists disagree and have no signed revision IDs to resolve the conflict.
    // All fixture integers fit in a one-byte varint.
    let mut bytes = vec![2, 5];
    for (id, key_byte) in [(0, 0xaa), (1, 0xbb), (2, 0xcc), (8, 0xbb), (3, 0xdd)] {
        bytes.extend([key_byte; 32]);
        bytes.push(id);
    }
    bytes.extend([3, 0, 1, 1, 1, 1, 10, 1, 2, 8, 10, 1, 3, 0]);
    bytes
}

#[test]
fn rejected_legacy_snapshot_preserves_personal_data_and_seed_choice() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().join("reader");
    let me = Keys::generate();
    let friend = Keys::generate();
    setup(&home, &me, &friend)?;
    let config = fs::read(home.join("config.json"))?;
    let personal = fs::read(home.join("social.json"))?;
    let snapshot = conflicting_legacy_snapshot();
    assert!(matches!(
        SocialGraph::from_binary(&"aa".repeat(32), &snapshot),
        Err(SocialGraphError::ConflictingFollowLists(1))
    ));

    // Haps imports signed Nostr events, not serialized graph snapshots.
    let input = tmp.path().join("legacy.bin");
    fs::write(&input, &snapshot)?;
    assert!(
        !cli(&home, &["import", input.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(fs::read(&input)?, snapshot);
    assert_eq!(fs::read(home.join("social.json"))?, personal);
    assert_eq!(fs::read(home.join("config.json"))?, config);
    ok(&home, &["starting-point"]);
    assert_eq!(fs::read(home.join("social.json"))?, personal);

    // Rejected persisted data must stop startup, never become a fresh seed.
    // Check both an explicitly disabled seed and a previously selected seed.
    for selected in [None, Some(friend.public_key().to_hex())] {
        if let Some(key) = selected {
            ok(&home, &["starting-point", &key]);
        }
        let before = fs::read(home.join("config.json"))?;
        fs::write(home.join("social.json"), &snapshot)?;
        assert!(!cli(&home, &["starting-point", "--clear"]).status.success());
        assert_eq!(fs::read(home.join("social.json"))?, snapshot);
        assert_eq!(fs::read(home.join("config.json"))?, before);
        // Only this fixture restores its own saved bytes for the next case.
        fs::write(home.join("social.json"), &personal)?;
    }
    Ok(())
}

#[test]
fn signed_empty_lists_and_revision_ids_survive_reopen_and_stale_import() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().join("reader");
    let me = Keys::generate();
    let friend = Keys::generate();
    let previous = setup(&home, &me, &friend)?;
    let current = vec![
        list(&me, Kind::ContactList, &[], 11),
        list(&me, Kind::MuteList, &[], 11),
    ];
    import(&home, &current)?;
    let saved = fs::read(home.join("social.json"))?;
    let config = fs::read(home.join("config.json"))?;
    import(&home, &previous)?;
    ok(&home, &["starting-point"]);
    assert_eq!(fs::read(home.join("social.json"))?, saved);
    assert_eq!(fs::read(home.join("config.json"))?, config);
    let events: Vec<Event> = serde_json::from_slice(&saved)?;
    assert_eq!(events.len(), 2);
    assert!(current.iter().all(|event| events.contains(event)));
    let mut trust = Trust::new(me.public_key().to_hex());
    for event in events {
        trust.ingest(event)?;
    }
    assert_eq!(trust.distance(&friend.public_key().to_hex()), None);
    assert!(!trust.muted(&friend.public_key().to_hex()));
    Ok(())
}
