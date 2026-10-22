//! CLI discovery and audit policy for exact native dependency closures.
use crate::{
    AuditOptions, Candidate, Config, candidates, ensure_audited, refresh_discovery,
    refresh_feedback,
};
use anyhow::{Context, Result, ensure};
use haps::{
    dependencies,
    install::{InstallRequest, Installation},
    trust::Trust,
};
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn install(
    home: &Path,
    config: &mut Config,
    trust: &mut Trust,
    installation: &Installation,
    root: &Candidate,
    options: AuditOptions<'_>,
    allow_foreign_untrusted: bool,
) -> Result<()> {
    let mut found = BTreeMap::from([(root.release.event.id.to_hex(), root.clone())]);
    let mut pending = VecDeque::from(root.release.data.package.dependencies.clone());
    let mut pins = BTreeMap::from([(root.release.identity(), root.release.event.id.to_hex())]);
    while let Some(dependency) = pending.pop_front() {
        let identity = dependency.identity()?;
        if let Some(previous) = pins.get(&identity) {
            ensure!(
                previous == &dependency.release,
                "conflicting dependency pins for {identity}"
            );
        } else {
            ensure!(
                pins.len() < dependencies::MAX_PACKAGES,
                "dependency graph exceeds package limit"
            );
            pins.insert(identity.clone(), dependency.release.clone());
        }
        if found.contains_key(&dependency.release) {
            continue;
        }
        let (publisher, name) = identity
            .split_once('/')
            .context("invalid dependency identity")?;
        refresh_discovery(
            home,
            config,
            trust,
            Some(nostr::PublicKey::parse(publisher)?),
            Some(name),
            None,
        )
        .await?;
        let candidate = candidates(home, config, None, Some(&identity))
            .await?
            .into_iter()
            .find(|c| {
                c.release.event.id.to_hex() == dependency.release
                    && c.release.identity() == identity
            })
            .with_context(|| {
                format!("exact dependency release unavailable or discovery incomplete: {identity}")
            })?;
        pending.extend(candidate.release.data.package.dependencies.clone());
        found.insert(dependency.release, candidate);
    }
    let available: Vec<_> = found.values().map(|c| c.release.clone()).collect();
    let ordered = dependencies::resolve(&root.release, &available)?;
    let receipts = installation.receipts()?;
    let options = AuditOptions {
        minimum: options.minimum.max(
            receipts
                .get(&root.release.identity())
                .map_or(0, |r| r.minimum_attestations),
        ),
        ..options
    };
    if ordered.len() > 1 {
        refresh_feedback(home, config, trust, &ordered).await?;
    }
    // Dependency discovery can bring in new warnings about the root as well.
    if options.allow_unaudited {
        trust.authorize_with_policy(
            &root.release,
            options.allow_untrusted,
            0,
            options.allow_warnings,
        )?;
    } else {
        trust.authorize_install(
            &root.release,
            options.allow_untrusted,
            options.minimum,
            options.allow_warnings,
        )?;
    }
    let mut requests = Vec::new();
    for release in &ordered {
        let candidate = &found[&release.event.id.to_hex()];
        let minimum = options.minimum.max(
            receipts
                .get(&release.identity())
                .map_or(0, |r| r.minimum_attestations),
        );
        if release.event.id != root.release.event.id {
            ensure_audited(
                home,
                config,
                trust,
                candidate,
                AuditOptions {
                    minimum,
                    // Choosing one publisher does not implicitly trust every other
                    // publisher referenced by that publisher's package.
                    allow_untrusted: allow_foreign_untrusted
                        || (options.allow_untrusted && release.author() == root.release.author()),
                    ..options
                },
            )
            .await
            .with_context(|| format!("dependency {} was not authorized", release.identity()))?;
        }
        requests.push(InstallRequest {
            repository: &candidate.repository,
            release: &candidate.release,
            minimum_attestations: minimum,
        });
    }
    installation.install_plan(&requests).await
}
