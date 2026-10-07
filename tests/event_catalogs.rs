use haps::{
    discovery::{Announcement, Discovery},
    event_catalog::IndexAnnouncement,
    model::{PackageSpec, target},
    repository::Repository,
};
use nostr::{Event, EventBuilder, Keys, Tag};
use std::{collections::BTreeMap, fs, path::Path, process::Command};

fn cli(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_haps"))
        .env("HAPS_HOME", home)
        .env("HAPS_NO_DEFAULTS", "true")
        .env_remove("NOSTR_RELAYS")
        .env("HTREE_CONFIG_DIR", home.join("htree"))
        .args(args)
        .output()
        .unwrap()
}
fn ok(home: &Path, args: &[&str]) -> String {
    let result = cli(home, args);
    assert!(
        result.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

#[test]
fn add_defaults_to_own_catalog_and_remove_does_not_install_or_publish() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("home");
    let payload = temp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("hello"), b"hello")?;
    let manifest = temp.path().join("haps.toml");
    fs::write(
        &manifest,
        format!(
            "name='hello'\nversion='1.0.0'\ntarget='{}'\ndescription='Hello'\n[commands]\nhello='hello'\n",
            target()
        ),
    )?;
    ok(
        &home,
        &[
            "add",
            manifest.to_str().unwrap(),
            "--payload",
            payload.to_str().unwrap(),
        ],
    );
    let events: Vec<Event> = serde_json::from_str(&ok(&home, &["catalog", "show"]))?;
    assert_eq!(events.len(), 1);
    let release = haps::model::Release::verify(events[0].clone())?;
    assert_eq!(release.author(), ok(&home, &["identity", "show"]).trim());
    assert!(!home.join("installed.json").exists());
    assert!(!home.join("discovery/outbox").exists());
    assert!(ok(&home, &["catalog", "list"]).contains("default"));
    ok(&home, &["catalog", "remove", "hello"]);
    let events: Vec<Event> = serde_json::from_str(&ok(&home, &["catalog", "show"]))?;
    assert!(events.is_empty());
    assert!(
        !cli(&home, &["catalog", "show", "../escape"])
            .status
            .success()
    );
    Ok(())
}

#[tokio::test]
async fn curator_preserves_original_events_and_lookup_merges_independent_indexes()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let alice = Keys::generate();
    let bob = Keys::generate();
    let curator = Keys::generate();
    let mut heads = Vec::new();
    for (author, name) in [(&alice, "alpha"), (&bob, "beta")] {
        let spec = PackageSpec {
            name: name.into(),
            version: "1.0.0".parse()?,
            target: target().into(),
            description: "Friendly package".into(),
            commands: BTreeMap::from([(name.into(), "app".into())]),
            source: None,
            app: None,
            desktop: None,
        };
        // Legacy metadata remains useful in a selected event index too.
        heads.push(Announcement::sign(
            author,
            &spec,
            "https://packages.example.test",
        )?);
    }
    let reader = temp.path().join("reader");
    let global = Discovery::open(&reader)?;
    global.ingest(heads.clone()).await?;
    drop(global);
    ok(
        &reader,
        &["add", &format!("{}/alpha", alice.public_key().to_hex())],
    );
    let selected: Vec<Event> = serde_json::from_str(&ok(&reader, &["catalog", "show"]))?;
    assert_eq!(
        selected,
        vec![heads[0].clone()],
        "browsing another package must not publish it"
    );
    let selected = Discovery::open(&reader.join("catalogs/default"))?;
    let first = temp.path().join("first");
    selected.export_index(&first, &curator).await?;
    let second = temp.path().join("second");
    let other = Discovery::open(&temp.path().join("other"))?;
    other.ingest(heads.clone()).await?;
    other.export_index(&second, &bob).await?;
    let lookup = haps::lookup::Lookup::default()
        .index(
            "first",
            std::sync::Arc::new(haps::index_reader::IndexReader::new(
                &reader,
                first.to_str().unwrap(),
                &curator.public_key().to_hex(),
            )),
        )?
        .index(
            "second",
            std::sync::Arc::new(haps::index_reader::IndexReader::new(
                &reader,
                second.to_str().unwrap(),
                &bob.public_key().to_hex(),
            )),
        )?;
    let events = lookup
        .query(vec![
            nostr::Filter::new()
                .kind(haps::discovery::SOFTWARE_KIND)
                .search("friendly"),
        ])
        .await?;
    assert_eq!(
        events.len(),
        2,
        "indexes are additive and duplicate events appear once"
    );
    assert!(events.contains(&heads[0]) && events.contains(&heads[1]));
    Ok(())
}

#[tokio::test]
async fn direct_head_cannot_change_publisher_release_or_platform() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let author = Keys::generate();
    let impostor = Keys::generate();
    let payload = temp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("app"), b"payload")?;
    let repo = Repository::local(temp.path().join("repo"))?;
    let spec = PackageSpec {
        name: "hello".into(),
        version: "1.0.0".parse()?,
        target: target().into(),
        description: "Hello".into(),
        commands: BTreeMap::from([("hello".into(), "app".into())]),
        source: None,
        app: None,
        desktop: None,
    };
    let release = repo.publish(&author, spec, &payload).await?;
    let snapshot = repo.catalog(&author.public_key().to_hex()).await?;
    let event = Announcement::sign_direct(
        &author,
        &repo,
        &snapshot,
        "hello",
        "https://packages.example.test",
    )
    .await?;
    let announcement = Announcement::verify(event.clone())?;
    let head = announcement.head.unwrap();
    // Curation can use the original signed release already supplied by an index;
    // it must not require the publisher's content server to be online.
    let reader = temp.path().join("reader");
    let cached = Discovery::open(&reader)?;
    cached
        .ingest([event.clone(), release.event.clone()])
        .await?;
    drop(cached);
    haps::event_catalog::add_package(&reader, "favorites", "hello").await?;
    let selected = Discovery::open(&reader.join("catalogs/favorites"))?
        .events(vec![nostr::Filter::new()])
        .await?;
    assert_eq!(selected.len(), 2);
    assert!(selected.contains(&release.event) && selected.contains(&event));
    let pointer = &head.releases[0];
    assert!(
        head.verify_release(
            pointer,
            release.event.clone(),
            &impostor.public_key().to_hex(),
            "hello"
        )
        .is_err()
    );
    let mut wrong = pointer.clone();
    wrong.target = "other-platform".into();
    assert!(
        head.verify_release(
            &wrong,
            release.event.clone(),
            &author.public_key().to_hex(),
            "hello"
        )
        .is_err()
    );
    assert!(
        head.verify_release(
            pointer,
            release.event,
            &author.public_key().to_hex(),
            "other-name"
        )
        .is_err()
    );
    let mut data: serde_json::Value = serde_json::from_str(&event.content)?;
    data["payload"] = "htree://npub1invalid/mutable".into();
    let forged = EventBuilder::new(event.kind, data.to_string())
        .tags(event.tags.clone())
        .sign_with_keys(&author)?;
    assert!(Announcement::verify(forged).is_err());
    let advanced =
        haps::event_catalog::advance(&author, event.clone(), std::slice::from_ref(&event))?;
    assert!(advanced.created_at > event.created_at);
    // Invalid catalog advertisements are rejected before being retained.
    let bad = EventBuilder::new(haps::model::APP_KIND, "{}")
        .tags([Tag::identifier("haps/index/../bad")])
        .sign_with_keys(&author)?;
    assert!(IndexAnnouncement::verify(bad).is_err());
    let good = IndexAnnouncement::sign(
        &author,
        "apps",
        &format!("htree://{}", hashtree_core::nhash_encode(&[1; 32])?),
    )?;
    assert_eq!(IndexAnnouncement::verify(good)?.name, "apps");
    Ok(())
}
