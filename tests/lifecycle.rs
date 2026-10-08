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
        source: None,
        app: None,
        desktop: None,
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
    assert_eq!(
        trust.followed_by_friends(&author.public_key().to_hex()),
        vec![friend.public_key().to_hex()]
    );
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
    let revocation = haps::trust::attest_at(
        &friend,
        first.event.id,
        false,
        "Regression".into(),
        haps::trust::attestation_time_ms(&approval)? + 1,
    )?;
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
    trust.ingest(
        EventBuilder::new(Kind::MuteList, "")
            .tags([Tag::public_key(friend.public_key())])
            .custom_created_at(nostr::Timestamp::from(
                nostr::Timestamp::now().as_secs() + 1,
            ))
            .sign_with_keys(&me)?,
    )?;
    assert!(
        trust
            .followed_by_friends(&author.public_key().to_hex())
            .is_empty()
    );
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

#[test]
fn starting_point_can_be_replaced_or_disabled_without_fabricated_events() -> anyhow::Result<()> {
    let me = Keys::generate();
    let first = Keys::generate().public_key().to_hex();
    let next = Keys::generate().public_key().to_hex();
    let mut trust = Trust::new(me.public_key().to_hex());
    trust.set_starting_point(Some(first.clone()))?;
    assert_eq!(trust.distance(&first), Some(1));
    assert!(trust.events().is_empty());
    trust.set_starting_point(Some(next.clone()))?;
    assert_eq!(trust.distance(&first), None);
    assert_eq!(trust.distance(&next), Some(1));
    trust.set_starting_point(None)?;
    assert_eq!(trust.distance(&next), None);
    Ok(())
}

#[tokio::test]
async fn packing_honors_nested_gitignores_and_common_junk_without_parent_rules()
-> anyhow::Result<()> {
    use haps::model::Manifest;
    let tmp = tempdir()?;
    fs::write(tmp.path().join(".gitignore"), "payload/\n")?;
    let payload = tmp.path().join("payload");
    let spec = package(&payload, "1.0.0");
    fs::write(payload.join(".gitignore"), "*.secret\n!public.secret\n")?;
    fs::write(payload.join("hidden.secret"), "ignored")?;
    fs::write(payload.join("public.secret"), "explicitly included")?;
    fs::write(payload.join(".DS_Store"), "junk")?;
    fs::write(payload.join(".env"), "private")?;
    fs::create_dir(payload.join("target"))?;
    fs::write(payload.join("target/cache"), "cache")?;
    fs::create_dir(payload.join("nested"))?;
    fs::write(payload.join("nested/.gitignore"), "ignore-me\n")?;
    fs::write(payload.join("nested/ignore-me"), "ignored")?;
    // Runtime dependencies inside an application must not be mistaken for a root build cache.
    fs::create_dir_all(payload.join("Example.app/node_modules"))?;
    fs::write(
        payload.join("Example.app/node_modules/runtime.js"),
        "runtime",
    )?;
    let repo = Repository::local(tmp.path().join("repo"))?;
    let release = repo.publish(&Keys::generate(), spec, &payload).await?;
    let files: Manifest = repo.json(&release.data.manifest).await?;
    let paths: Vec<_> = files.files.iter().map(|f| f.path.as_str()).collect();
    for ignored in [
        "hidden.secret",
        ".DS_Store",
        ".env",
        "target/cache",
        "nested/ignore-me",
    ] {
        assert!(!paths.contains(&ignored), "included {ignored}");
    }
    for included in [
        "bin/hello",
        "public.secret",
        "Example.app/node_modules/runtime.js",
    ] {
        assert!(paths.contains(&included), "excluded {included}");
    }
    fs::write(payload.join(".gitignore"), "[z-a]\n")?;
    let invalid = repo
        .publish(&Keys::generate(), package(&payload, "1.0.1"), &payload)
        .await;
    assert!(
        invalid.is_err(),
        "malformed ignore rules must fail packaging"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn desktop_launcher_tracks_install_update_rollback_and_removal() -> anyhow::Result<()> {
    use haps::model::DesktopEntry;
    let tmp = tempdir()?;
    let home = tmp.path().join("home with spaces $ and %f and \" quote");
    let entries = tmp.path().join("applications");
    let installed = Installation::new(home.clone())?.with_desktop_dir(Some(entries.clone()));
    let author = Keys::generate();
    let repo = Repository::local(tmp.path().join("repo"))?;
    let payload = tmp.path().join("payload");
    let marker = tmp.path().join("launched");
    let make = |version: &str| {
        let mut spec = package(&payload, version);
        fs::write(
            payload.join("bin/hello"),
            format!(
                "#!/bin/sh\nprintf '%s' \"$XDG_DATA_DIRS\" > '{}'\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::write(
            payload.join("icon.svg"),
            "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
        )
        .unwrap();
        spec.desktop = Some(DesktopEntry {
            name: "Hello Desktop".into(),
            command: "hello".into(),
            icon: "icon.svg".into(),
        });
        spec
    };
    let first = repo.publish(&author, make("1.0.0"), &payload).await?;
    installed.install(&repo, &first).await?;
    let entry = entries.join(haps::desktop::filename(&home.canonicalize()?, &first));
    let initial = fs::read_to_string(&entry)?;
    assert!(initial.contains(&first.event.id.to_hex()));
    assert!(initial.contains("Name=Hello Desktop\n"));
    if std::env::var_os("HAPS_TEST_DESKTOP").is_some() {
        let output = std::process::Command::new("gio")
            .env("XDG_DATA_DIRS", "/custom resources:/usr/share")
            .arg("launch")
            .arg(&entry)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            fs::read_to_string(&marker)?,
            format!(
                "{}/usr/share:{}/share:/custom resources:/usr/share",
                installed.path("hello")?.display(),
                installed.path("hello")?.display(),
            )
        );
    }
    // Reinstallation repairs a missing desktop entry without downloading again.
    fs::remove_file(&entry)?;
    installed.install(&repo, &first).await?;
    assert_eq!(fs::read_to_string(&entry)?, initial);
    // Repair the original space-escaped entry on a same-version reinstall.
    let space_escaped: String = initial
        .lines()
        .map(|line| {
            let line = if line.starts_with("Name=") || line.starts_with("Icon=") {
                line.replace(' ', "\\s")
            } else {
                line.into()
            };
            format!("{line}\n")
        })
        .collect();
    fs::write(&entry, &space_escaped)?;
    installed.install(&repo, &first).await?;
    assert_eq!(fs::read_to_string(&entry)?, initial);
    let second = repo.publish(&author, make("1.1.0"), &payload).await?;
    fs::write(&entry, "user edit")?;
    assert!(installed.install(&repo, &second).await.is_err());
    assert_eq!(installed.receipt("hello")?.current.id, first.event.id);
    assert_eq!(fs::read_to_string(&entry)?, "user edit");
    // A normal update also recognizes the space-escaped entry from the old release.
    fs::write(&entry, &space_escaped)?;
    installed.install(&repo, &second).await?;
    assert!(fs::read_to_string(&entry)?.contains(&second.event.id.to_hex()));
    installed.rollback("hello")?;
    assert_eq!(fs::read_to_string(&entry)?, initial);
    installed.remove("hello")?;
    assert!(!entry.exists());
    // A publisher removing desktop metadata also removes the old launcher.
    installed.install(&repo, &first).await?;
    let third = repo
        .publish(&author, package(&payload, "1.2.0"), &payload)
        .await?;
    installed.install(&repo, &third).await?;
    assert!(!entry.exists());
    Ok(())
}

#[test]
fn desktop_metadata_rejects_injection_and_undeclared_commands() {
    use haps::model::DesktopEntry;
    let tmp = tempdir().unwrap();
    let mut spec = package(tmp.path(), "1.0.0");
    spec.target = "x86_64-unknown-linux-gnu".into();
    spec.desktop = Some(DesktopEntry {
        name: "Hello".into(),
        command: "hello".into(),
        icon: "icon.png".into(),
    });
    assert!(spec.validate().is_ok());
    spec.desktop.as_mut().unwrap().name = "Hello\nExec=evil".into();
    assert!(spec.validate().is_err());
    spec.desktop.as_mut().unwrap().name = "Hello".into();
    spec.desktop.as_mut().unwrap().command = "sh -c evil".into();
    assert!(spec.validate().is_err());
    spec.desktop.as_mut().unwrap().command = "hello".into();
    spec.desktop.as_mut().unwrap().icon = "../icon.png".into();
    assert!(spec.validate().is_err());
}
