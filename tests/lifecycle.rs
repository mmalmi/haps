use haps::{
    install::Installation,
    model::{PackageSpec, target},
    repository::Repository,
    trust::Trust,
};
use nostr::{EventBuilder, Keys, Kind, Tag};
use std::{collections::BTreeMap, fs};
use tempfile::tempdir;

fn package(root: &std::path::Path, version: &str) -> PackageSpec {
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("bin/hello"), format!("hello {version}\n")).unwrap();
    PackageSpec {
        name: "hello".into(),
        version: version.parse().unwrap(),
        target: target().into(),
        description: "Friendly hello tool".into(),
        commands: BTreeMap::from([("hello".into(), "bin/hello".into())]),
    }
}

#[tokio::test]
async fn signed_publish_search_install_update_rollback_and_remove() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let author = Keys::generate();
    let reader = Keys::generate();
    let payload = tmp.path().join("payload");
    let repo = Repository::local(tmp.path().join("repository"))?;
    let first = repo
        .publish(&author, package(&payload, "1.0.0"), &payload)
        .await?;
    let catalog = repo.catalog(&author.public_key().to_hex()).await?;
    let results = repo.search(&catalog, "friendly").await?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].event.id, first.event.id);
    let mut trust = Trust::new(reader.public_key().to_hex());
    assert!(trust.authorize(&first, false, 0).is_err());
    trust.ingest(
        EventBuilder::new(Kind::ContactList, "")
            .tags([Tag::public_key(author.public_key())])
            .sign_with_keys(&reader)?,
    )?;
    assert_eq!(trust.distance(&author.public_key().to_hex()), Some(1));
    trust.authorize(&first, false, 0)?;
    let installed = Installation::new(tmp.path().join("home"))?;
    installed.install(&repo, &first).await?;
    assert_eq!(
        fs::read_to_string(installed.command("hello", None)?)?,
        "hello 1.0.0\n"
    );
    let second = repo
        .publish(&author, package(&payload, "1.1.0"), &payload)
        .await?;
    installed.install(&repo, &second).await?;
    assert_eq!(
        fs::read_to_string(installed.command("hello", None)?)?,
        "hello 1.1.0\n"
    );
    installed.rollback("hello")?;
    assert_eq!(
        fs::read_to_string(installed.command("hello", None)?)?,
        "hello 1.0.0\n"
    );
    installed.remove("hello")?;
    assert!(installed.command("hello", None).is_err());
    Ok(())
}

#[tokio::test]
async fn signatures_hashes_and_publisher_pins_are_enforced() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let author = Keys::generate();
    let payload = tmp.path().join("payload");
    let repo = Repository::local(tmp.path().join("repository"))?;
    let release = repo
        .publish(&author, package(&payload, "1.0.0"), &payload)
        .await?;
    assert!(
        repo.catalog(&Keys::generate().public_key().to_hex())
            .await
            .is_err()
    );
    let mut event = release.event.clone();
    event.content.push(' ');
    assert!(haps::model::Release::verify(event).is_err());
    assert!(
        repo.publish(&author, package(&payload, "1.0.0"), &payload)
            .await
            .is_err()
    );
    let root = hashtree_core::Cid::parse(&release.data.manifest)?;
    let block = repo
        .path()
        .unwrap()
        .join("blobs")
        .join(haps::store::block_path(&root.hash));
    fs::write(block, b"corrupted")?;
    // A new reader must reject corrupt content, not install it.
    let result = async {
        let catalog = repo.catalog(&author.public_key().to_hex()).await?;
        for candidate in repo.releases(&catalog).await? {
            Installation::new(tmp.path().join("home"))?
                .install(&repo, &candidate)
                .await?;
        }
        anyhow::Ok(())
    }
    .await;
    assert!(result.is_err());
    Ok(())
}

#[tokio::test]
async fn signed_social_graph_and_release_specific_attestations() -> anyhow::Result<()> {
    let me = Keys::generate();
    let friend = Keys::generate();
    let author = Keys::generate();
    let mut trust = Trust::new(me.public_key().to_hex());
    // Import in reverse graph order: distances must still be recomputed.
    trust.ingest(
        EventBuilder::new(Kind::ContactList, "")
            .tags([Tag::public_key(author.public_key())])
            .sign_with_keys(&friend)?,
    )?;
    trust.ingest(
        EventBuilder::new(Kind::ContactList, "")
            .tags([Tag::public_key(friend.public_key())])
            .sign_with_keys(&me)?,
    )?;
    assert_eq!(trust.distance(&author.public_key().to_hex()), Some(2));
    let mut forged = EventBuilder::new(Kind::ContactList, "").sign_with_keys(&author)?;
    forged.pubkey = me.public_key();
    assert!(trust.ingest(forged).is_err());

    let tmp = tempdir()?;
    let payload = tmp.path().join("payload");
    let repo = Repository::local(tmp.path().join("repo"))?;
    let first = repo
        .publish(&author, package(&payload, "1.0.0"), &payload)
        .await?;
    let second = repo
        .publish(&author, package(&payload, "1.1.0"), &payload)
        .await?;
    let approval = haps::trust::attest(&friend, first.event.id, true, "Tested".into())?;
    trust.ingest(approval.clone())?;
    trust.ingest(approval.clone())?;
    // Only one vote per reviewer, and only for the exact signed release.
    assert_eq!(trust.attesters(&first), vec![friend.public_key().to_hex()]);
    trust.authorize(&first, false, 1)?;
    assert!(trust.authorize(&first, true, 2).is_err());
    assert!(trust.authorize(&second, false, 1).is_err());
    trust.ingest(haps::trust::attest(
        &author,
        first.event.id,
        true,
        "Self review".into(),
    )?)?;
    assert_eq!(trust.attesters(&first).len(), 1);
    let revocation = haps::trust::attest(&friend, first.event.id, false, "Regression".into())?;
    let revocation = EventBuilder::new(revocation.kind, revocation.content)
        .tags(revocation.tags)
        .custom_created_at(nostr::Timestamp::from(approval.created_at.as_secs() + 1))
        .sign_with_keys(&friend)?;
    trust.ingest(revocation)?;
    trust.ingest(approval)?; // An old approval cannot undo a revocation.
    assert!(trust.authorize(&first, false, 1).is_err());
    trust.ingest(haps::trust::attest(
        &me,
        second.event.id,
        true,
        "Verified".into(),
    )?)?;
    trust.authorize(&second, false, 1)?;
    trust.ingest(
        EventBuilder::new(Kind::MuteList, "")
            .tags([Tag::public_key(author.public_key())])
            .sign_with_keys(&me)?,
    )?;
    assert!(trust.authorize(&second, true, 1).is_err());
    Ok(())
}

#[tokio::test]
async fn failed_update_and_signed_unsafe_manifest_preserve_installed_version() -> anyhow::Result<()>
{
    use haps::model::{Manifest, Release, release_tag, sign};
    let tmp = tempdir()?;
    let author = Keys::generate();
    let payload = tmp.path().join("payload");
    let repo = Repository::local(tmp.path().join("repo"))?;
    let first = repo
        .publish(&author, package(&payload, "1.0.0"), &payload)
        .await?;
    let installation = Installation::new(tmp.path().join("home"))?;
    installation.install(&repo, &first).await?;
    let next = repo
        .publish(&author, package(&payload, "1.1.0"), &payload)
        .await?;
    let mut manifest: Manifest = repo.json(&next.data.manifest).await?;
    let root = hashtree_core::Cid::parse(&manifest.files[0].cid)?;
    fs::write(
        repo.path()
            .unwrap()
            .join("blobs")
            .join(haps::store::block_path(&root.hash)),
        b"tampered",
    )?;
    assert!(installation.install(&repo, &next).await.is_err());
    assert_eq!(installation.receipt("hello")?.current.id, first.event.id);
    manifest.files[0].path = "../escape".into();
    let mut bad_data = next.data.clone();
    bad_data.manifest = repo.put_json(&manifest).await?;
    let signed_bad = Release::verify(sign(&author, &release_tag(&bad_data.package), &bad_data)?)?;
    assert!(installation.install(&repo, &signed_bad).await.is_err());
    assert_eq!(
        fs::read_to_string(installation.command("hello", None)?)?,
        "hello 1.0.0\n"
    );
    Ok(())
}

#[tokio::test]
async fn package_and_release_comments_reject_forgery_and_cross_thread_replies() -> anyhow::Result<()>
{
    use haps::{comments, model::sign};
    let tmp = tempdir()?;
    let author = Keys::generate();
    let reader = Keys::generate();
    let payload = tmp.path().join("payload");
    let repo = Repository::local(tmp.path().join("repo"))?;
    let release = repo
        .publish(&author, package(&payload, "1.0.0"), &payload)
        .await?;
    let card = sign(
        &author,
        "haps/package/hello",
        &serde_json::json!({"schema":"haps.package.v1", "name":"hello"}),
    )?;
    let comment = comments::create(&reader, &card, false, None, "Package discussion".into())?;
    let reply = comments::create(&author, &card, false, Some(&comment), "Reply".into())?;
    assert_eq!(
        comments::verify(&reply)?,
        comments::Scope::package(&release)
    );
    assert!(
        comments::create(
            &author,
            &release.event,
            true,
            Some(&comment),
            "Wrong thread".into()
        )
        .is_err()
    );
    let mut forged = comment;
    forged.content = "Forged comment".into();
    assert!(comments::Comments::default().ingest(forged).is_err());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn payload_symlinks_are_rejected_until_the_format_preserves_them() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let payload = tmp.path().join("payload");
    let spec = package(&payload, "1.0.0");
    std::os::unix::fs::symlink("hello", payload.join("bin/alias"))?;
    let repo = Repository::local(tmp.path().join("repo"))?;
    assert!(
        repo.publish(&Keys::generate(), spec, &payload)
            .await
            .is_err()
    );
    assert!(!repo.path().unwrap().join("catalog.json").exists());
    Ok(())
}

#[test]
fn paths_cannot_escape_on_any_supported_os() {
    for path in [
        "../escape",
        "/tmp/escape",
        "C:\\evil",
        "a/../../evil",
        "a\\..\\evil",
        "a:evil",
        "a//b",
        "a/./b",
        "CON",
        "a/NUL.txt",
        "a/b.",
    ] {
        assert!(haps::model::safe_path(path).is_err(), "accepted {path}");
    }
    assert!(haps::model::safe_path("Hello.app/Contents/MacOS/hello").is_ok());
}
