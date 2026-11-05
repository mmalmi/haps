//! A package address points to exact signed releases and content-addressed blocks.
use crate::{
    model::*,
    repository::{Repository, Snapshot},
};
use anyhow::{Context, Result, ensure};
use nostr::{Event, EventId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const MAX_RELEASE_POINTERS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasePointer {
    pub id: String,
    pub cid: String,
    pub version: semver::Version,
    pub target: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageHead {
    pub schema: String,
    pub description: String,
    pub payload: String,
    pub card: String,
    pub releases: Vec<ReleasePointer>,
}

impl PackageHead {
    pub async fn from_catalog(
        repo: &Repository,
        snapshot: &Snapshot,
        author: &str,
        name: &str,
        payload: &str,
    ) -> Result<Self> {
        let mut releases = Vec::new();
        let mut coordinates = BTreeSet::new();
        for cid in &snapshot.catalog.releases {
            let release = Release::verify(repo.json(cid).await?)?;
            if release.author() == author && release.data.package.name == name {
                ensure!(
                    coordinates.insert((
                        release.data.package.version.clone(),
                        release.data.package.target.clone()
                    )),
                    "duplicate release coordinate"
                );
                releases.push((release, cid.clone()));
            }
        }
        let latest = releases
            .iter()
            .max_by_key(|(release, _)| &release.data.package.version)
            .context("package is not in this catalog")?;
        let mut head = Self {
            schema: "haps.package-head.v1".into(),
            description: latest.0.data.package.description.clone(),
            payload: payload.into(),
            card: snapshot
                .catalog
                .packages
                .get(&format!("{author}/{name}"))
                .context("package card is missing")?
                .clone(),
            releases: Vec::new(),
        };
        let budget = crate::discovery::MAX_SOFTWARE_CONTENT
            .checked_sub(serde_json::to_vec(&head)?.len())
            .context("package metadata exceeds the announcement size limit")?;
        head.releases = bounded_releases(
            releases
                .iter()
                .map(|(release, cid)| ReleasePointer {
                    id: release.event.id.to_hex(),
                    cid: cid.clone(),
                    version: release.data.package.version.clone(),
                    target: release.data.package.target.clone(),
                })
                .collect(),
            budget,
        )?;
        head.validate()?;
        Ok(head)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == "haps.package-head.v1",
            "unsupported package head"
        );
        crate::discovery::validate_location(&self.payload)?;
        if self.payload.starts_with("htree://") {
            ensure!(
                self.payload.starts_with("htree://nhash"),
                "package payload must use an immutable Hashtree hash"
            );
        }
        hashtree_core::Cid::parse(&self.card)?;
        ensure!(
            !self.releases.is_empty() && self.releases.len() <= MAX_RELEASE_POINTERS,
            "package head requires 1–64 release pointers"
        );
        let mut coordinates = BTreeSet::new();
        for release in &self.releases {
            ensure!(
                EventId::from_hex(&release.id)?.to_hex() == release.id,
                "invalid release event ID"
            );
            hashtree_core::Cid::parse(&release.cid)?;
            ensure!(
                coordinates.insert((&release.version, &release.target)),
                "duplicate release coordinate"
            );
            ensure!(
                !release.target.is_empty() && release.target.len() <= 128,
                "invalid release target"
            );
        }
        Ok(())
    }

    pub fn verify_release(
        &self,
        pointer: &ReleasePointer,
        event: Event,
        author: &str,
        name: &str,
    ) -> Result<Release> {
        let release = Release::verify(event)?;
        ensure!(
            release.event.id.to_hex() == pointer.id
                && release.author() == author
                && release.data.package.name == name
                && release.data.package.version == pointer.version
                && release.data.package.target == pointer.target,
            "package head and signed release disagree"
        );
        Ok(release)
    }
}

/// Announcements are bounded summaries; the signed catalog retains every release.
fn bounded_releases(releases: Vec<ReleasePointer>, budget: usize) -> Result<Vec<ReleasePointer>> {
    let sizes: Vec<_> = releases
        .iter()
        .map(|release| serde_json::to_vec(release).map(|bytes| bytes.len()))
        .collect::<std::result::Result<_, _>>()?;
    if releases.len() <= MAX_RELEASE_POINTERS
        && sizes.iter().sum::<usize>() + releases.len().saturating_sub(1) <= budget
    {
        return Ok(releases);
    }
    let mut newest: Vec<_> = (0..releases.len()).collect();
    newest.sort_by(|&a, &b| {
        releases[b]
            .version
            .cmp(&releases[a].version)
            .then_with(|| releases[a].target.cmp(&releases[b].target))
            .then_with(|| releases[a].id.cmp(&releases[b].id))
    });
    let mut targets = BTreeSet::new();
    let mut stable_targets = BTreeSet::new();
    let mut selected = BTreeSet::new();
    for &index in &newest {
        let release = &releases[index];
        if targets.insert(&release.target) {
            selected.insert(index);
        }
        // Frequent previews must not displace the installable stable release,
        // and active platforms must not displace less frequently updated ones.
        if release.version.pre.is_empty() && stable_targets.insert(&release.target) {
            selected.insert(index);
        }
    }
    let mut used = selected.iter().map(|&index| sizes[index]).sum::<usize>()
        + selected.len().saturating_sub(1);
    ensure!(
        selected.len() <= MAX_RELEASE_POINTERS && used <= budget,
        "latest releases across package platforms exceed the announcement limits"
    );
    for index in newest {
        if selected.contains(&index) {
            continue;
        }
        if selected.len() == MAX_RELEASE_POINTERS {
            break;
        }
        let added = sizes[index] + usize::from(!selected.is_empty());
        if used + added > budget {
            break;
        }
        used += added;
        selected.insert(index);
    }
    Ok(releases
        .into_iter()
        .enumerate()
        .filter_map(|(index, release)| selected.contains(&index).then_some(release))
        .collect())
}
