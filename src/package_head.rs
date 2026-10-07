//! A package address points to exact signed releases and content-addressed blocks.
use crate::{
    model::*,
    repository::{Repository, Snapshot},
};
use anyhow::{Context, Result, ensure};
use nostr::{Event, EventId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

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
        for cid in &snapshot.catalog.releases {
            let release = Release::verify(repo.json(cid).await?)?;
            if release.author() == author && release.data.package.name == name {
                releases.push((release, cid.clone()));
            }
        }
        let latest = releases
            .iter()
            .max_by_key(|(release, _)| &release.data.package.version)
            .context("package is not in this catalog")?;
        let head = Self {
            schema: "haps.package-head.v1".into(),
            description: latest.0.data.package.description.clone(),
            payload: payload.into(),
            card: snapshot
                .catalog
                .packages
                .get(&format!("{author}/{name}"))
                .context("package card is missing")?
                .clone(),
            releases: releases
                .iter()
                .map(|(release, cid)| ReleasePointer {
                    id: release.event.id.to_hex(),
                    cid: cid.clone(),
                    version: release.data.package.version.clone(),
                    target: release.data.package.target.clone(),
                })
                .collect(),
        };
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
            !self.releases.is_empty() && self.releases.len() <= 64,
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
