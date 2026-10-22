//! A reviewed local build has its own signer and payload, never the publisher's signature.
use crate::{AuditReviewArgs, Candidate, Config, audit_agent, publishing_keys, record_audit};
use anyhow::{Context, Result, ensure};
use haps::{
    audit::Evidence, build::Checkout, progress::Progress, repository::Repository, trust::Trust,
};
use sha2::{Digest, Sha256};
use std::{io::IsTerminal, path::Path, sync::Arc};

#[allow(clippy::too_many_arguments)]
pub(super) async fn from_release(
    home: &Path,
    config: &mut Config,
    trust: &mut Trust,
    upstream: &Candidate,
    options: &AuditReviewArgs,
    recipe_file: Option<&Path>,
    non_interactive: bool,
) -> Result<Candidate> {
    let (progress, _reporter) = Progress::stderr(true)?;
    progress.stage("Fetching pinned source to build locally");
    let source = upstream
        .release
        .data
        .package
        .source
        .clone()
        .context("this release does not declare pinned source")?;
    let source = haps::build::SourceCheckout::fetch(source, &home.join("builds"))?;
    let mut options = options.clone();
    let recipe = if let Some(path) = recipe_file {
        ensure!(
            std::fs::metadata(path)?.len() <= haps::model::MAX_METADATA as u64,
            "build manifest is too large"
        );
        haps::build::manifest::Manifest::select(
            &std::fs::read_to_string(path)?,
            haps::model::target(),
            Some(&upstream.release.data.package.name),
        )?
        .recipe()?
    } else if let Some(recipe) =
        source.recipe_for(None, Some(&upstream.release.data.package.name))?
    {
        recipe
    } else {
        progress.clear();
        let interactive =
            !non_interactive && std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        let agent = audit_agent(
            options
                .audit_agent
                .as_deref()
                .or(config.audit.agent.as_deref()),
            interactive,
        )?;
        let model = options
            .audit_model
            .as_deref()
            .or(config.audit.model.as_deref());
        let recipe =
            haps::audit::plan_build(&source, &upstream.release.data.package, &agent, model)?;
        options.audit_agent = Some(agent);
        recipe
    };
    let checkout = Checkout::with_recipe(source, recipe)?;
    ensure!(
        checkout.recipe.package == upstream.release.data.package,
        "source manifest does not describe the selected package"
    );
    progress.clear();
    build(home, config, trust, checkout, &options, non_interactive).await
}

pub(super) async fn build(
    home: &Path,
    config: &mut Config,
    trust: &mut Trust,
    checkout: Checkout,
    options: &AuditReviewArgs,
    non_interactive: bool,
) -> Result<Candidate> {
    let interactive =
        !non_interactive && std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let agent = audit_agent(
        options
            .audit_agent
            .as_deref()
            .or(config.audit.agent.as_deref()),
        interactive,
    )?;
    let model = options.audit_model.clone().or_else(|| {
        (agent != "manual")
            .then(|| config.audit.model.clone())
            .flatten()
    });
    let keys = publishing_keys(home, config)?;
    let review = haps::audit::review(&checkout, &agent, model.as_deref(), interactive)?;
    eprintln!(
        "Reviewed source and build instructions; building locally. No publisher binary will be installed."
    );
    let (progress, _reporter) = Progress::stderr(true)?;
    let scan = if haps::security::enabled(home) {
        progress.stage("Scanning reviewed source before building");
        Some(haps::security::scan_source(
            home,
            checkout
                .recipe
                .package
                .source
                .as_ref()
                .context("source missing")?,
            &checkout.root(),
        )?)
    } else {
        None
    };
    let recipe_bytes = serde_json::to_vec(&checkout.recipe)?;
    let recipe_hash = hex::encode(Sha256::digest(&recipe_bytes));
    progress.stage("Building reviewed source");
    let payload = checkout.execute()?;
    progress.stage("Recording the local build");
    let repo = Repository::local(home.join("built-packages"))?;
    let source_name = format!("local-builds-{}", &keys.public_key().to_hex()[..16]);
    let location = repo
        .path()
        .context("local repository missing")?
        .canonicalize()?
        .to_str()
        .context("non-UTF8 build repository path")?
        .to_owned();
    if let Some(existing) = config.sources.get(&source_name) {
        ensure!(
            existing.author == keys.public_key().to_hex() && existing.location == location,
            "local build catalog name is already used by another source"
        );
    }
    let mut package = checkout.recipe.package.clone();
    let manifest = repo.pack_payload(&package, &payload).await?;
    // Include both inputs and output so two local builds cannot silently replace
    // one immutable release when a toolchain or dependency changes its output.
    let build_id = hex::encode(Sha256::digest(format!("{recipe_hash}:{manifest}")));
    package.version.build = semver::BuildMetadata::new(&format!("haps.{}", &build_id[..20]))?;
    let release = repo.publish_reusable(&keys, package, &payload).await?;
    let recipe_cid = repo.put_json(&checkout.recipe).await?;
    let evidence = Evidence {
        schema: "haps.audit.v1".into(),
        method: format!("{}-review-local-build", review.reviewer.agent),
        provenance: format!(
            "Built locally from reviewed pinned source {} and recipe {} (SHA-256 {}). No publisher binary was compared. Dependencies and toolchain are not independently verified.",
            checkout
                .recipe
                .package
                .source
                .as_ref()
                .context("source missing")?
                .rev,
            recipe_cid,
            recipe_hash
        ),
        binary_match: false,
        reviewer: Some(review.reviewer),
    };
    if let Some(scan) = scan {
        haps::security::bind_report(home, &release, scan)?;
    }
    progress.clear();
    record_audit(
        home,
        config,
        trust,
        &keys,
        &release,
        (&evidence, review.note),
        (options.publish_audit, interactive),
    )
    .await?;
    let snapshot = repo.catalog(&keys.public_key().to_hex()).await?;
    config.sources.insert(
        source_name,
        crate::Source {
            location,
            author: keys.public_key().to_hex(),
            sequence: snapshot.head.sequence,
            event_id: snapshot.event.id.to_hex(),
        },
    );
    crate::save_config(home, config)?;
    Ok(Candidate {
        repository: Arc::new(repo),
        package_card: None,
        release,
    })
}
