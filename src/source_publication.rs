//! Source must be publicly readable before packages referring to it are announced.
use crate::{
    build::{SourceCheckout, git},
    model::SourceInfo,
};
use anyhow::{Context, Result, ensure};
use nostr::PublicKey;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub(crate) fn ensure_available<'a>(
    sources: impl Iterator<Item = &'a SourceInfo>,
    publisher: PublicKey,
    repository: &Path,
) -> Result<()> {
    let mut grouped = BTreeMap::<String, BTreeSet<String>>::new();
    for source in sources.filter(|source| source.git.starts_with("htree://")) {
        source.validate()?;
        grouped
            .entry(source.git.clone())
            .or_default()
            .insert(source.rev.clone());
    }
    if grouped.is_empty() {
        return Ok(());
    }
    let work = tempfile::tempdir()?;
    for (source_url, revisions) in grouped {
        let url = reqwest::Url::parse(&source_url)?;
        let owned = url
            .host_str()
            .and_then(|host| PublicKey::parse(host).ok())
            .is_some_and(|owner| owner == publisher);
        // Push only the advertised commit and its ancestors, never the working
        // tree, unrelated refs, or another publisher's repository.
        let mut refs = Vec::new();
        if owned {
            for rev in &revisions {
                let local = git()
                    .current_dir(repository)
                    .args(["cat-file", "-e", &format!("{rev}^{{commit}}")])
                    .output()?;
                if local.status.success() {
                    refs.push(format!("{rev}:refs/tags/haps-source/{rev}"));
                }
            }
        }
        if !refs.is_empty() {
            eprintln!("Publishing {} source revision(s) to Hashtree", refs.len());
            let mut push = git();
            crate::helpers::configure_git_publisher(&mut push)?;
            let status = push
                .current_dir(repository)
                .args(["push", "--", &source_url])
                .args(&refs)
                .status()
                .context("could not publish source to Hashtree")?;
            ensure!(
                status.success(),
                "source upload failed; packages were not announced"
            );
        }
        eprintln!(
            "Checking {} source revision(s) from one empty-cache clone",
            revisions.len()
        );
        SourceCheckout::verify_public_revisions(&source_url, &revisions, work.path())
            .context("public source is incomplete; packages were not announced. Publish the pinned source and its complete Git history to Hashtree, then retry")?;
    }
    Ok(())
}
