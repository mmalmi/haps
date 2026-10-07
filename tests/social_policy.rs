use haps::{
    model::{PackageSpec, target},
    repository::Repository,
    trust::{Trust, attest_at},
};
use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp, nips::nip19::ToBech32};
use std::{collections::BTreeMap, fs, path::Path, process::Command};

fn relations(author: &Keys, kind: Kind, people: &[&Keys], at: u64) -> Event {
    EventBuilder::new(kind, "")
        .tags(people.iter().map(|key| Tag::public_key(key.public_key())))
        .custom_created_at(Timestamp::from(at))
        .sign_with_keys(author)
        .unwrap()
}

async fn release(root: &Path, author: &Keys) -> anyhow::Result<haps::model::Release> {
    let payload = root.join("payload");
    fs::create_dir_all(&payload)?;
    fs::write(payload.join("data"), b"social policy fixture")?;
    Repository::local(root.join("repo"))?
        .publish(
            author,
            PackageSpec {
                name: "hello".into(),
                version: "1.0.0".parse()?,
                target: target().into(),
                description: "Social policy fixture".into(),
                commands: BTreeMap::new(),
                app: None,
                desktop: None,
                source: None,
            },
            &payload,
        )
        .await
}

#[tokio::test]
async fn reachable_authors_and_vouchers_use_shared_iris_overmute_policy() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let me = Keys::generate();
    let friend = Keys::generate();
    let critic = Keys::generate();
    let voucher = Keys::generate();
    let stranger = Keys::generate();
    let author = Keys::generate();
    let release = release(tmp.path(), &author).await?;
    let at = Timestamp::now().as_secs();
    let mut trust = Trust::new(me.public_key().to_hex());
    trust.ingest(relations(&me, Kind::ContactList, &[&friend, &critic], at))?;
    trust.ingest(relations(
        &friend,
        Kind::ContactList,
        &[&voucher, &author],
        at,
    ))?;
    assert_eq!(trust.distance(&author.public_key().to_hex()), Some(2));
    trust.authorize(&release, false, 0)?;

    // Opinions from unreachable keys cannot hide a publisher.
    trust.ingest(relations(&stranger, Kind::MuteList, &[&author], at))?;
    trust.authorize(&release, false, 0)?;
    // At Iris's recommendation threshold (3), one nearby mute beats one follow.
    trust.ingest(relations(&critic, Kind::MuteList, &[&author], at))?;
    assert!(trust.authorize(&release, false, 0).is_err());
    trust.authorize(&release, true, 0)?;
    // A reachable, non-overmuted voucher can approve this exact release.
    trust.ingest(attest_at(
        &voucher,
        release.event.id,
        true,
        "Checked".into(),
        at * 1000,
    )?)?;
    trust.authorize(&release, false, 0)?;
    trust.authorize(&release, false, 1)?;
    // Overmuting the voucher removes its authority too.
    trust.ingest(relations(
        &critic,
        Kind::MuteList,
        &[&author, &voucher],
        at + 1,
    ))?;
    assert!(trust.attesters(&release).is_empty());
    assert!(trust.authorize(&release, false, 0).is_err());
    assert!(trust.authorize(&release, true, 1).is_err());
    // Your own direct follow is the nearest opinion, ahead of friends' mutes.
    trust.ingest(relations(
        &me,
        Kind::ContactList,
        &[&friend, &critic, &author],
        at + 1,
    ))?;
    trust.authorize(&release, false, 0)?;
    trust.ingest(relations(&me, Kind::MuteList, &[&author], at + 1))?;
    assert!(trust.authorize(&release, true, 0).is_err());
    Ok(())
}

#[tokio::test]
async fn cli_hides_unknown_packages_but_explicit_publisher_installs_warn() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let author = Keys::generate();
    let me = Keys::generate();
    let friend = Keys::generate();
    let release = release(tmp.path(), &author).await?;
    let home = tmp.path().join("reader");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_haps"))
            .env("HAPS_HOME", &home)
            .env("HAPS_NO_DEFAULTS", "true")
            .env("HTREE_CONFIG_DIR", home.join("htree"))
            .env_remove("NOSTR_RELAYS")
            .args(args)
            .output()
            .unwrap()
    };
    assert!(
        run(&["identity", "use", &me.public_key().to_hex()])
            .status
            .success()
    );
    assert!(
        run(&[
            "source",
            "add",
            "fixture",
            tmp.path().join("repo").to_str().unwrap(),
            "--author",
            &author.public_key().to_hex()
        ])
        .status
        .success()
    );
    let search = run(&["search", "hello", "--json"]);
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&search.stdout)?,
        serde_json::json!([])
    );
    assert!(!run(&["install", "hello", "--json"]).status.success());
    let explicit = format!("{}/hello", author.public_key().to_bech32()?);
    let installed = run(&["install", &explicit, "--json"]);
    assert!(
        installed.status.success(),
        "{} {}",
        String::from_utf8_lossy(&installed.stderr),
        String::from_utf8_lossy(&installed.stdout)
    );
    assert!(
        String::from_utf8_lossy(&installed.stderr)
            .contains("not authored or vouched for by your social graph")
    );
    // An explicit install does not change the graph or make later bare-name updates trusted.
    assert!(!run(&["update", "hello", "--json"]).status.success());
    let at = Timestamp::now().as_secs();
    let import = tmp.path().join("events.json");
    fs::write(
        &import,
        serde_json::to_vec(&[
            relations(&me, Kind::ContactList, &[&friend], at),
            attest_at(&friend, release.event.id, true, "Checked".into(), at * 1000)?,
        ])?,
    )?;
    assert!(run(&["import", import.to_str().unwrap()]).status.success());
    let search = run(&["search", "hello", "--json"]);
    let found: serde_json::Value = serde_json::from_slice(&search.stdout)?;
    assert_eq!(found.as_array().unwrap().len(), 1);
    assert!(run(&["install", "hello", "--json"]).status.success());
    // A vouch for 1.0.0 must not approve a new build or silently select the old one.
    let mut newer = release.data.package.clone();
    newer.version = "1.1.0".parse()?;
    Repository::local(tmp.path().join("repo"))?
        .publish(&author, newer, &tmp.path().join("payload"))
        .await?;
    assert!(!run(&["install", "hello", "--json"]).status.success());
    assert!(
        run(&["install", "hello", "--version", "1.0.0", "--json"])
            .status
            .success()
    );
    fs::write(
        &import,
        serde_json::to_vec(&attest_at(
            &friend,
            release.event.id,
            false,
            "Withdrawn".into(),
            at * 1000 + 1,
        )?)?,
    )?;
    assert!(run(&["import", import.to_str().unwrap()]).status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&run(&["search", "hello", "--json"]).stdout)?,
        serde_json::json!([])
    );
    assert!(!run(&["install", "hello", "--json"]).status.success());
    Ok(())
}
