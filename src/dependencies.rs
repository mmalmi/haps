//! Publisher-qualified, exact-release native dependencies. No version guessing.
use crate::model::{Release, safe_name};
use anyhow::{Context, Result, ensure};
use nostr::{EventId, PublicKey};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_PACKAGES: usize = 128;
const MAX_DEPTH: usize = 32;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    /// Public key (hex or npub) and package name. Local aliases are not identities.
    pub package: String,
    /// Exact signed release event ID, including its platform and manifest.
    pub release: String,
}

impl Dependency {
    pub fn identity(&self) -> Result<String> {
        let (publisher, name) = self
            .package
            .split_once('/')
            .context("dependency must use publisher/package")?;
        safe_name(name)?;
        Ok(format!(
            "{}/{}",
            PublicKey::parse(publisher)?.to_hex(),
            name
        ))
    }
    pub fn validate(&self) -> Result<()> {
        self.identity()?;
        ensure!(
            EventId::from_hex(&self.release)?.to_hex() == self.release,
            "dependency release must be a lowercase hexadecimal event ID"
        );
        Ok(())
    }
    pub fn matches(&self, release: &Release) -> Result<bool> {
        Ok(self.identity()? == release.identity() && self.release == release.event.id.to_hex())
    }
}

/// Verify and order a bounded closure, dependencies first. Reject even signed
/// publisher substitutions, platform changes, identity cycles and conflicting pins.
pub fn resolve(root: &Release, available: &[Release]) -> Result<Vec<Release>> {
    let mut releases = BTreeMap::new();
    for release in available.iter().chain(std::iter::once(root)) {
        let release = Release::verify(release.event.clone())?;
        releases.insert(release.event.id.to_hex(), release);
    }
    let root = releases
        .get(&root.event.id.to_hex())
        .context("missing root release")?;
    let mut plan = Planner {
        releases: &releases,
        target: &root.data.package.target,
        selected: BTreeMap::new(),
        visiting: BTreeSet::new(),
        ordered: Vec::new(),
    };
    plan.visit(root, 0)?;
    Ok(plan.ordered)
}

struct Planner<'a> {
    releases: &'a BTreeMap<String, Release>,
    target: &'a str,
    selected: BTreeMap<String, EventId>,
    visiting: BTreeSet<String>,
    ordered: Vec<Release>,
}

impl Planner<'_> {
    fn visit(&mut self, release: &Release, depth: usize) -> Result<()> {
        ensure!(
            depth <= MAX_DEPTH,
            "dependency graph exceeds {MAX_DEPTH} levels"
        );
        let identity = release.identity();
        ensure!(
            !self.visiting.contains(&identity),
            "dependency cycle at {identity}"
        );
        if let Some(previous) = self.selected.get(&identity) {
            ensure!(
                *previous == release.event.id,
                "conflicting dependency pins for {identity}"
            );
            return Ok(());
        }
        ensure!(
            self.selected.len() < MAX_PACKAGES,
            "dependency graph exceeds {MAX_PACKAGES} packages"
        );
        ensure!(
            release.data.package.target == self.target,
            "dependency target differs from its parent: {identity}"
        );
        self.selected.insert(identity.clone(), release.event.id);
        self.visiting.insert(identity.clone());
        for dependency in &release.data.package.dependencies {
            let child = self.releases.get(&dependency.release).with_context(|| {
                format!(
                    "exact dependency release unavailable: {}",
                    dependency.package
                )
            })?;
            ensure!(
                dependency.matches(child)?,
                "dependency publisher/package does not match its signed release"
            );
            self.visit(child, depth + 1)?;
        }
        self.visiting.remove(&identity);
        self.ordered.push(release.clone());
        Ok(())
    }
}

/// Validate the entire proposed installed set before updating any registrations.
pub fn validate_installed(releases: &[Release]) -> Result<()> {
    let installed: BTreeMap<_, _> = releases.iter().map(|r| (r.identity(), r)).collect();
    for parent in releases {
        for dependency in &parent.data.package.dependencies {
            let identity = dependency.identity()?;
            let child = installed.get(&identity).with_context(|| {
                format!(
                    "{} requires dependency {identity}; it cannot be missing or removed",
                    parent.identity()
                )
            })?;
            ensure!(
                dependency.matches(child)?,
                "{} requires a different pinned release of {identity}; update dependent packages together",
                parent.identity()
            );
        }
        if !parent.data.package.dependencies.is_empty() {
            resolve(parent, releases)?;
        }
    }
    Ok(())
}
