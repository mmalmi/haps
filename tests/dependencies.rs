use haps::{
    dependencies::{self, Dependency},
    install::{InstallRequest, Installation},
    model::{PackageSpec, Release, target},
    repository::Repository,
};
use std::{collections::BTreeMap, fs};

struct Fixture {
    temp: tempfile::TempDir,
    keys: nostr::Keys,
    repo: Repository,
    installed: Installation,
}
impl Fixture {
    fn new() -> anyhow::Result<Self> {
        let temp = tempfile::tempdir()?;
        Ok(Self {
            repo: Repository::local(temp.path().join("repository"))?,
            installed: Installation::new(temp.path().join("home"))?.with_desktop_dir(None),
            keys: nostr::Keys::generate(),
            temp,
        })
    }
    async fn publish(
        &self,
        name: &str,
        version: &str,
        dependencies: Vec<Dependency>,
    ) -> anyhow::Result<Release> {
        let payload = self.temp.path().join(format!("{name}-{version}"));
        fs::create_dir(&payload)?;
        fs::write(payload.join(name), format!("{name} {version}"))?;
        self.repo
            .publish(
                &self.keys,
                PackageSpec {
                    name: name.into(),
                    version: version.parse()?,
                    target: target().into(),
                    description: "Dependency fixture".into(),
                    commands: BTreeMap::from([(name.into(), name.into())]),
                    dependencies,
                    source: None,
                    app: None,
                    desktop: None,
                },
                &payload,
            )
            .await
    }
    async fn install(&self, releases: &[&Release]) -> anyhow::Result<()> {
        let requests: Vec<_> = releases
            .iter()
            .map(|release| InstallRequest {
                repository: &self.repo,
                release,
                minimum_attestations: 2,
            })
            .collect();
        self.installed.install_plan(&requests).await
    }
}
fn pin(release: &Release) -> Dependency {
    Dependency {
        package: release.identity(),
        release: release.event.id.to_hex(),
    }
}

#[test]
fn existing_manifests_remain_unchanged_and_dependency_identities_are_explicit() -> anyhow::Result<()>
{
    let value = serde_json::json!({"name":"old", "version":"1.0.0", "target":target(), "description":"Old package", "commands":{}});
    let mut spec: PackageSpec = serde_json::from_value(value.clone())?;
    assert!(spec.dependencies.is_empty());
    assert_eq!(serde_json::to_value(&spec)?, value);
    spec.dependencies = vec![Dependency {
        package: "alias/tool".into(),
        release: "0".repeat(64),
    }];
    assert!(spec.validate().is_err());
    spec.dependencies[0].package =
        format!("{}/tool", nostr::Keys::generate().public_key().to_hex());
    spec.validate()?;
    spec.dependencies.push(spec.dependencies[0].clone());
    assert!(
        spec.validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate")
    );
    Ok(())
}

#[tokio::test]
async fn resolution_deduplicates_transitive_pins_and_rejects_conflicts_and_substitution()
-> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let tool = fixture.publish("tool", "1.0.0", vec![]).await?;
    let left = fixture.publish("left", "1.0.0", vec![pin(&tool)]).await?;
    let right = fixture.publish("right", "1.0.0", vec![pin(&tool)]).await?;
    let root = fixture
        .publish("root", "1.0.0", vec![pin(&left), pin(&right)])
        .await?;
    let plan = dependencies::resolve(&root, &[left.clone(), right.clone(), tool.clone()])?;
    assert_eq!(plan.len(), 4);
    assert_eq!(plan.first().unwrap().event.id, tool.event.id);
    assert_eq!(plan.last().unwrap().event.id, root.event.id);
    assert!(dependencies::resolve(&root, &[]).is_err());
    let tool2 = fixture.publish("tool", "2.0.0", vec![]).await?;
    let right2 = fixture.publish("right", "2.0.0", vec![pin(&tool2)]).await?;
    let conflict = fixture
        .publish("root", "2.0.0", vec![pin(&left), pin(&right2)])
        .await?;
    assert!(
        dependencies::resolve(&conflict, &[left, right2, tool.clone(), tool2])
            .unwrap_err()
            .to_string()
            .contains("conflicting")
    );
    let mut wrong = pin(&tool);
    wrong.package = format!("{}/tool", nostr::Keys::generate().public_key().to_hex());
    let forged = fixture.publish("forged", "1.0.0", vec![wrong]).await?;
    assert!(dependencies::resolve(&forged, std::slice::from_ref(&tool)).is_err());
    let cycle = fixture.publish("tool", "3.0.0", vec![pin(&right)]).await?;
    assert!(
        dependencies::resolve(&cycle, &[right, tool])
            .unwrap_err()
            .to_string()
            .contains("cycle")
    );
    Ok(())
}

#[tokio::test]
async fn install_update_rollback_and_removal_preserve_the_whole_dependency_set()
-> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let tool1 = fixture.publish("tool", "1.0.0", vec![]).await?;
    let root1 = fixture.publish("root", "1.0.0", vec![pin(&tool1)]).await?;
    assert!(
        fixture
            .installed
            .install(&fixture.repo, &root1)
            .await
            .is_err()
    );
    assert!(fixture.installed.receipts()?.is_empty());
    fixture.install(&[&tool1, &root1]).await?;
    assert!(fixture.installed.remove("tool").is_err());
    let tool2 = fixture.publish("tool", "2.0.0", vec![]).await?;
    assert!(fixture.install(&[&tool2]).await.is_err());
    let root2 = fixture.publish("root", "2.0.0", vec![pin(&tool2)]).await?;
    fixture.install(&[&root2, &tool2]).await?;
    let authorized = fixture.installed.rollback_plan("root")?;
    fixture.installed.rollback("root")?;
    assert!(
        fixture
            .installed
            .rollback_checked("root", &authorized)
            .is_err(),
        "a concurrent change must not activate a different, unaudited closure"
    );
    assert_eq!(
        fixture.installed.receipt("root")?.current.id,
        root1.event.id
    );
    assert_eq!(
        fixture.installed.receipt("tool")?.current.id,
        tool1.event.id
    );
    assert_eq!(fixture.installed.receipt("tool")?.minimum_attestations, 2);
    fixture.installed.rollback("root")?;
    assert_eq!(
        fixture.installed.receipt("tool")?.current.id,
        tool2.event.id
    );
    fixture.installed.remove("root")?;
    fixture.installed.remove("tool")?;
    assert!(fixture.installed.receipts()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn failed_download_and_registration_do_not_partly_update_dependencies() -> anyhow::Result<()>
{
    let fixture = Fixture::new()?;
    let tool1 = fixture.publish("tool", "1.0.0", vec![]).await?;
    let root1 = fixture.publish("root", "1.0.0", vec![pin(&tool1)]).await?;
    fixture.install(&[&tool1, &root1]).await?;
    let tool2 = fixture.publish("tool", "2.0.0", vec![]).await?;
    let root2 = fixture.publish("root", "2.0.0", vec![pin(&tool2)]).await?;
    let manifest = hashtree_core::Cid::parse(&root2.data.manifest)?;
    let blob = fixture
        .repo
        .path()
        .unwrap()
        .join("blobs")
        .join(haps::store::block_path(&manifest.hash));
    let original = fs::read(&blob)?;
    fs::write(&blob, b"corrupt")?;
    assert!(fixture.install(&[&tool2, &root2]).await.is_err());
    assert_eq!(
        fixture.installed.receipt("tool")?.current.id,
        tool1.event.id
    );
    assert_eq!(
        fixture.installed.receipt("root")?.current.id,
        root1.event.id
    );
    fs::write(blob, original)?;
    let bin = fixture.installed.link("root", None)?;
    fixture.installed.link("tool", Some(&bin))?;
    let suffix = if cfg!(windows) { ".cmd" } else { "" };
    let root_link = bin.join(format!("root{suffix}"));
    let old_link = fs::read(&root_link)?;
    fs::write(bin.join(format!("tool{suffix}")), "foreign command")?;
    assert!(fixture.install(&[&tool2, &root2]).await.is_err());
    assert_eq!(fs::read(root_link)?, old_link);
    assert_eq!(
        fixture.installed.receipt("root")?.current.id,
        root1.event.id
    );
    assert_eq!(
        fixture.installed.receipt("tool")?.current.id,
        tool1.event.id
    );
    Ok(())
}

#[tokio::test]
async fn shared_pins_block_incompatible_rollback_and_target_changes() -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let tool1 = fixture.publish("tool", "1.0.0", vec![]).await?;
    let root1 = fixture.publish("root", "1.0.0", vec![pin(&tool1)]).await?;
    fixture.install(&[&tool1, &root1]).await?;
    let tool2 = fixture.publish("tool", "2.0.0", vec![]).await?;
    let root2 = fixture.publish("root", "2.0.0", vec![pin(&tool2)]).await?;
    let other = fixture.publish("other", "1.0.0", vec![pin(&tool2)]).await?;
    fixture.install(&[&tool2, &root2, &other]).await?;
    assert!(fixture.installed.rollback("root").is_err());
    assert_eq!(
        fixture.installed.receipt("root")?.current.id,
        root2.event.id
    );
    assert_eq!(
        fixture.installed.receipt("tool")?.current.id,
        tool2.event.id
    );
    let mut foreign_spec = tool2.data.package.clone();
    foreign_spec.target = "other-platform".into();
    let foreign = fixture
        .repo
        .publish(
            &fixture.keys,
            foreign_spec,
            &fixture.temp.path().join("tool-2.0.0"),
        )
        .await?;
    let parent = fixture
        .publish("foreign-parent", "1.0.0", vec![pin(&foreign)])
        .await?;
    assert!(
        dependencies::resolve(&parent, &[foreign])
            .unwrap_err()
            .to_string()
            .contains("target")
    );
    Ok(())
}

#[tokio::test]
async fn dependency_depth_is_bounded_before_activation() -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let mut releases = Vec::new();
    for index in 0..34 {
        let deps = releases.last().map(|r| vec![pin(r)]).unwrap_or_default();
        releases.push(
            fixture
                .publish(&format!("part-{index}"), "1.0.0", deps)
                .await?,
        );
    }
    let root = releases.last().unwrap();
    assert!(
        dependencies::resolve(root, &releases)
            .unwrap_err()
            .to_string()
            .contains("levels")
    );
    assert!(fixture.installed.receipts()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn private_environments_reject_extra_commands_and_name_collisions() -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let tool = fixture.publish("tool", "1.0.0", vec![]).await?;
    let root = fixture.publish("root", "1.0.0", vec![pin(&tool)]).await?;
    fixture.install(&[&root, &tool]).await?;
    let environment = fixture
        .temp
        .path()
        .join("home/environments")
        .join(root.event.id.to_hex())
        .join("bin");
    fs::write(environment.join("unexpected"), "foreign command")?;
    assert!(
        fixture
            .install(&[&root, &tool])
            .await
            .unwrap_err()
            .to_string()
            .contains("unexpected")
    );
    let mut spec = tool.data.package.clone();
    spec.name = "collision".into();
    spec.dependencies = vec![pin(&tool)];
    let collision = fixture
        .repo
        .publish(&fixture.keys, spec, &fixture.temp.path().join("tool-1.0.0"))
        .await?;
    assert!(
        fixture
            .install(&[&collision])
            .await
            .unwrap_err()
            .to_string()
            .contains("command conflicts")
    );
    assert!(fixture.installed.receipt("collision").is_err());
    Ok(())
}
