//! Source must be publicly readable before packages referring to it are announced.
use crate::{
    build::{SourceCheckout, git},
    model::SourceInfo,
};
use anyhow::{Context, Result, ensure};
use nostr::PublicKey;
use std::{collections::BTreeSet, path::Path};

pub(crate) fn ensure_available<'a>(
    sources: impl Iterator<Item = &'a SourceInfo>,
    publisher: PublicKey,
    repository: &Path,
) -> Result<()> {
    let sources: BTreeSet<_> = sources
        .filter(|source| source.git.starts_with("htree://"))
        .map(|source| (source.git.clone(), source.rev.clone()))
        .collect();
    if sources.is_empty() {
        return Ok(());
    }
    let work = tempfile::tempdir()?;
    for (url, rev) in sources {
        let source = SourceInfo { git: url, rev };
        source.validate()?;
        let url = reqwest::Url::parse(&source.git)?;
        let owned = url
            .host_str()
            .and_then(|host| PublicKey::parse(host).ok())
            .is_some_and(|owner| owner == publisher);
        // Push only the advertised commit and its ancestors, never the working
        // tree, unrelated refs, or another publisher's repository.
        let local = git()
            .current_dir(repository)
            .args(["cat-file", "-e", &format!("{}^{{commit}}", source.rev)])
            .output()?;
        if owned && local.status.success() {
            eprintln!("Publishing source revision {} to Hashtree", source.rev);
            let mut push = git();
            crate::helpers::configure_git(&mut push)?;
            let status = push
                .current_dir(repository)
                .args([
                    "push",
                    "--",
                    &source.git,
                    &format!("{}:refs/tags/haps-source/{}", source.rev, source.rev),
                ])
                .status()
                .context("could not publish source to Hashtree")?;
            ensure!(
                status.success(),
                "source upload failed; packages were not announced"
            );
        }
        eprintln!(
            "Checking source revision {} from an empty cache",
            source.rev
        );
        SourceCheckout::fetch_public(source, work.path())
            .context("public source is incomplete; packages were not announced. Publish the pinned source and its complete Git history to Hashtree, then retry")?;
    }
    Ok(())
}
