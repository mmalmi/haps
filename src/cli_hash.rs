//! Immutable package lookup never substitutes a newer named catalog.
use crate::*;

pub(super) fn reference(value: &str) -> Result<Option<String>> {
    let hash = value.strip_prefix("htree://").unwrap_or(value);
    if value.starts_with("htree://") || hash.starts_with("nhash1") {
        ensure!(
            hash.starts_with("nhash1"),
            "install by hash requires nhash… or htree://nhash…"
        );
        hashtree_core::nhash_decode(hash).context("invalid immutable package hash")?;
        return Ok(Some(format!("htree://{hash}")));
    }
    Ok(None)
}

pub(super) async fn candidates(home: &Path, reference: &str) -> Result<Vec<Candidate>> {
    let repository = Arc::new(Repository::open(reference, &home.join("cache"))?);
    let snapshot = repository.signed_catalog().await
        .context("this hash must identify a Haps package catalog containing signed catalog.json metadata; a raw binary or payload hash is not installable")?;
    Ok(repository
        .releases(&snapshot)
        .await?
        .into_iter()
        .map(|release| Candidate {
            package_card: snapshot.catalog.packages.get(&release.identity()).cloned(),
            repository: repository.clone(),
            release,
        })
        .collect())
}

pub(super) fn package(
    choices: &[Candidate],
    selected: Option<&str>,
    version: Option<&semver::Version>,
) -> Result<String> {
    let identities: BTreeSet<_> = choices
        .iter()
        .filter(|candidate| {
            let spec = &candidate.release.data.package;
            spec.target == target()
                && version.is_none_or(|v| &spec.version == v)
                && selected
                    .is_none_or(|name| candidate.release.identity() == name || spec.name == name)
        })
        .map(|candidate| candidate.release.identity())
        .collect();
    ensure!(
        !identities.is_empty(),
        "no matching package for {} in this immutable catalog",
        target()
    );
    ensure!(
        identities.len() == 1,
        "this hash contains several packages; choose one with --select publisher/name:\n  {}",
        identities.iter().cloned().collect::<Vec<_>>().join("\n  ")
    );
    Ok(identities.into_iter().next().unwrap())
}
