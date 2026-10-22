use super::*;
use std::collections::BTreeSet;

pub struct InstallRequest<'a> {
    pub repository: &'a Repository,
    pub release: &'a Release,
    /// The caller must authorize every release before installation.
    pub minimum_attestations: usize,
}

impl Installation {
    pub(super) fn validate_receipts(receipts: &BTreeMap<String, Receipt>) -> Result<()> {
        let releases = receipts
            .values()
            .map(|r| Release::verify(r.current.clone()))
            .collect::<Result<Vec<_>>>()?;
        crate::dependencies::validate_installed(&releases)
    }

    /// Stage the complete verified plan, then change launchers and receipts as
    /// one transaction. A missing dependency never leaves a partly installed set.
    pub async fn install_plan(&self, requests: &[InstallRequest<'_>]) -> Result<()> {
        ensure!(
            !requests.is_empty() && requests.len() <= crate::dependencies::MAX_PACKAGES,
            "invalid installation plan size"
        );
        let _guard = lock_with_wait(&self.home.join(".install.lock"), || {
            self.progress
                .stage("Waiting for another package installation");
        })?;
        let before = self.receipts()?;
        let mut after = before.clone();
        let mut ids = BTreeSet::new();
        let mut releases = Vec::new();
        for request in requests {
            let release = Release::verify(request.release.event.clone())?;
            ensure!(
                release.data.package.target == target(),
                "release target does not match this machine ({})",
                target()
            );
            let identity = release.identity();
            ensure!(
                ids.insert(identity.clone()),
                "duplicate package in installation plan"
            );
            let old = before.get(&identity);
            let unchanged = old.is_some_and(|r| r.current.id == release.event.id);
            if let Some(old) = old.filter(|_| !unchanged) {
                ensure!(
                    release.data.package.version
                        > Release::verify(old.current.clone())?.data.package.version,
                    "refusing downgrade or changed release at the same version for {identity}; use rollback"
                );
            }
            after.insert(
                identity,
                Receipt {
                    current: release.event.clone(),
                    previous: if unchanged {
                        old.and_then(|r| r.previous.clone())
                    } else {
                        old.map(|r| r.current.clone())
                    },
                    minimum_attestations: request
                        .minimum_attestations
                        .max(old.map_or(0, |r| r.minimum_attestations)),
                },
            );
            releases.push((release, unchanged));
        }
        Self::validate_receipts(&after)?;
        for ((release, unchanged), request) in releases.iter().zip(requests) {
            if !unchanged {
                self.stage_payload(request.repository, release).await?;
            }
        }
        let available = after
            .values()
            .map(|r| Release::verify(r.current.clone()))
            .collect::<Result<Vec<_>>>()?;
        for (release, _) in &releases {
            self.prepare_environment(release, &available)?;
        }
        if releases.iter().all(|(_, unchanged)| *unchanged) {
            self.progress
                .stage("Package is already current; checking registration");
        } else {
            self.progress.stage("Registering package installation");
        }
        self.commit_set(&before, &after, &ids.into_iter().collect::<Vec<_>>(), 0)
    }

    // Nest the existing ownership-aware launcher transactions. Any failure
    // unwinds every changed launcher; the receipt set is saved only at the end.
    fn commit_set(
        &self,
        before: &BTreeMap<String, Receipt>,
        after: &BTreeMap<String, Receipt>,
        ids: &[String],
        index: usize,
    ) -> Result<()> {
        let Some(identity) = ids.get(index) else {
            return self.save(after);
        };
        self.change(
            before.get(identity).map(|r| &r.current),
            after.get(identity).map(|r| &r.current),
            || self.commit_set(before, after, ids, index + 1),
        )
    }

    /// The exact retained releases that a rollback would activate, including
    /// dependencies. Callers can apply the audit policy to all of them first.
    pub fn rollback_plan(&self, name: &str) -> Result<Vec<Release>> {
        Self::retained_plan(&self.receipts()?, name)
    }

    fn retained_plan(receipts: &BTreeMap<String, Receipt>, name: &str) -> Result<Vec<Release>> {
        let receipt = Self::find(receipts, name)?.1;
        let root = Release::verify(
            receipt
                .previous
                .clone()
                .context("no previous version retained")?,
        )?;
        let mut retained = Vec::new();
        for receipt in receipts.values() {
            retained.push(Release::verify(receipt.current.clone())?);
            if let Some(previous) = &receipt.previous {
                retained.push(Release::verify(previous.clone())?);
            }
        }
        crate::dependencies::resolve(&root, &retained)
    }

    pub(super) fn rollback_dependencies(
        &self,
        name: &str,
        expected: Option<&[Release]>,
    ) -> Result<()> {
        let _guard = lock(&self.home.join(".install.lock"))?;
        let before = self.receipts()?;
        let releases = Self::retained_plan(&before, name)?;
        if let Some(expected) = expected {
            ensure!(
                releases
                    .iter()
                    .map(|r| r.event.id)
                    .eq(expected.iter().map(|r| r.event.id)),
                "installation changed after rollback authorization; retry rollback"
            );
        }
        let mut after = before.clone();
        let mut ids = Vec::new();
        for release in &releases {
            ensure!(
                self.version_dir(release).is_dir(),
                "previous dependency payload is unavailable: {}",
                release.identity()
            );
            let identity = release.identity();
            let old = before
                .get(&identity)
                .context("dependency receipt is unavailable")?;
            if old.current.id == release.event.id {
                continue;
            }
            after.insert(
                identity.clone(),
                Receipt {
                    current: release.event.clone(),
                    previous: Some(old.current.clone()),
                    minimum_attestations: old.minimum_attestations,
                },
            );
            ids.push(identity);
        }
        Self::validate_receipts(&after)?;
        let available = after
            .values()
            .map(|r| Release::verify(r.current.clone()))
            .collect::<Result<Vec<_>>>()?;
        for release in &releases {
            self.prepare_environment(release, &available)?;
        }
        self.commit_set(&before, &after, &ids, 0)
    }
}
