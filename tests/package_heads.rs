use haps::{
    discovery::Announcement,
    model::{PackageSpec, Release},
    repository::Repository,
};
use nostr::Keys;
use std::{collections::BTreeMap, fs, path::Path};

async fn publish(
    repo: &Repository,
    keys: &Keys,
    payload: &Path,
    version: &str,
    target: &str,
) -> anyhow::Result<Release> {
    repo.publish(
        keys,
        PackageSpec {
            name: "many-releases".into(),
            version: version.parse()?,
            target: target.into(),
            description: "A package with retained history".into(),
            commands: BTreeMap::new(),
            dependencies: vec![],
            source: None,
            app: None,
            desktop: None,
        },
        payload,
    )
    .await
}

#[tokio::test]
async fn oversized_catalog_publishes_without_losing_signed_history() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let payload = temp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("data"), "retained payload")?;
    let repo = Repository::local(temp.path().join("catalog"))?;
    let keys = Keys::generate();
    let mut published = Vec::new();
    for version in 13..=25 {
        for target in 0..5 {
            published.push(
                publish(
                    &repo,
                    &keys,
                    &payload,
                    &format!("0.1.{version}"),
                    &format!("platform-{target}"),
                )
                .await?,
            );
        }
    }
    let snapshot = repo.catalog(&keys.public_key().to_hex()).await?;
    let original = fs::read(repo.path().unwrap().join("catalog.json"))?;
    let announcement = Announcement::verify(
        Announcement::sign_direct(
            &keys,
            &repo,
            &snapshot,
            "many-releases",
            "https://example.test/catalog",
        )
        .await?,
    )?;
    let head = announcement.head.unwrap();
    assert!((5..=64).contains(&head.releases.len()));
    assert!(serde_json::to_vec(&head)?.len() <= 16_384);
    for release in &published[60..] {
        assert!(
            head.releases
                .iter()
                .any(|pointer| pointer.id == release.event.id.to_hex())
        );
    }
    assert_eq!(
        fs::read(repo.path().unwrap().join("catalog.json"))?,
        original
    );
    assert_eq!(repo.releases(&snapshot).await?.len(), 65);
    let historical = published
        .iter()
        .find(|release| {
            !head
                .releases
                .iter()
                .any(|pointer| pointer.id == release.event.id.to_hex())
        })
        .unwrap();
    assert_eq!(
        repo.exact_release(
            &snapshot,
            &historical.identity(),
            &historical.data.package.version,
            &historical.data.package.target
        )
        .await?
        .unwrap()
        .event,
        historical.event
    );
    Ok(())
}

#[tokio::test]
async fn bounded_heads_keep_stable_releases_and_infrequent_platforms() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let payload = temp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("data"), "payload")?;
    let repo = Repository::local(temp.path().join("catalog"))?;
    let keys = Keys::generate();
    let stable = publish(&repo, &keys, &payload, "1.0.0", "frequent-platform").await?;
    let rare = publish(&repo, &keys, &payload, "0.5.0", "infrequent-platform").await?;
    let mut latest = None;
    for prerelease in 0..66 {
        latest = Some(
            publish(
                &repo,
                &keys,
                &payload,
                &format!("2.0.0-rc.{prerelease}"),
                "frequent-platform",
            )
            .await?,
        );
    }
    let snapshot = repo.catalog(&keys.public_key().to_hex()).await?;
    let event = Announcement::sign_direct(
        &keys,
        &repo,
        &snapshot,
        "many-releases",
        "https://example.test/catalog",
    )
    .await?;
    let head = Announcement::verify(event)?.head.unwrap();
    assert!((3..=64).contains(&head.releases.len()));
    assert!(serde_json::to_vec(&head)?.len() <= 16_384);
    for release in [stable, rare, latest.unwrap()] {
        assert!(
            head.releases
                .iter()
                .any(|pointer| pointer.id == release.event.id.to_hex())
        );
    }
    assert_eq!(repo.releases(&snapshot).await?.len(), 68);
    Ok(())
}

#[tokio::test]
async fn announcement_size_also_bounds_histories_below_64_releases() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let payload = temp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("data"), "payload")?;
    let repo = Repository::local(temp.path().join("catalog"))?;
    let keys = Keys::generate();
    for version in 0..45 {
        publish(
            &repo,
            &keys,
            &payload,
            &format!("1.0.{version}"),
            "platform",
        )
        .await?;
    }
    let mut spec = repo
        .releases(&repo.catalog(&keys.public_key().to_hex()).await?)
        .await?[0]
        .data
        .package
        .clone();
    spec.version = "1.0.45".parse()?;
    spec.description = "\"".repeat(4096);
    let latest = repo.publish(&keys, spec, &payload).await?;
    let snapshot = repo.catalog(&keys.public_key().to_hex()).await?;
    let event = Announcement::sign_direct(
        &keys,
        &repo,
        &snapshot,
        "many-releases",
        "https://example.test/catalog",
    )
    .await?;
    let head = Announcement::verify(event)?.head.unwrap();
    assert!(!head.releases.is_empty() && head.releases.len() < 46);
    assert!(serde_json::to_vec(&head)?.len() <= 16_384);
    assert!(
        head.releases
            .iter()
            .any(|pointer| pointer.id == latest.event.id.to_hex())
    );
    assert_eq!(repo.releases(&snapshot).await?.len(), 46);
    Ok(())
}
