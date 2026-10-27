//! Installation runs behind a boxed boundary to bound the command dispatch stack.
use crate::*;

pub(super) async fn install(
    home: &Path,
    config: &mut Config,
    trust: &mut Trust,
    installation: &Installation,
    progress: &haps::progress::Progress,
    command: Command,
    non_interactive: bool,
) -> Result<()> {
    let Command::Install {
        package,
        select_package,
        from_source,
        recipe_file,
        version,
        release_id,
        allow_untrusted,
        allow_warnings,
        require_attestations,
        allow_unaudited,
        mut audit,
        audit_review,
        json,
    } = command
    else {
        unreachable!("install dispatcher")
    };

    let immutable = cli_hash::reference(&package)?;
    ensure!(
        immutable.is_some() || select_package.is_none(),
        "--select is only used when installing by hash"
    );
    progress.stage("Reading package catalogs");
    let mut choices = if let Some(reference) = &immutable {
        cli_hash::candidates(home, reference).await?
    } else {
        candidates(home, config, None, Some(&package)).await?
    };
    if let Some(expected) = release_id {
        choices.retain(|candidate| candidate.release.event.id == expected);
        ensure!(
            !choices.is_empty(),
            "the selected release changed; refresh the package list before installing"
        );
    }
    let package = if immutable.is_some() {
        let selected = select_package
            .as_deref()
            .map(|name| resolve_package(config, name))
            .transpose()?;
        cli_hash::package(&choices, selected.as_deref(), version.as_ref())?
    } else {
        package
    };
    let pinned_candidates = if immutable.is_some() {
        choices.clone()
    } else {
        Vec::new()
    };
    let releases: Vec<_> = choices.iter().map(|c| c.release.clone()).collect();
    progress.stage("Checking release attestations and warnings");
    refresh_feedback(home, config, trust, &releases).await?;
    progress.clear();
    let candidate = select(
        choices,
        &package,
        version.as_ref(),
        &config.aliases,
        trust,
        target(),
        Selection {
            non_interactive,
            include_untrusted: allow_untrusted,
            confirm_install: true,
        },
    )?;
    ensure!(
        !from_source || candidate.release.data.package.source.is_some(),
        "this release does not declare pinned source"
    );
    let require_attestations = require_attestations.max(1).max(
        installation
            .receipts()?
            .get(&candidate.release.identity())
            .map_or(0, |r| r.minimum_attestations),
    );
    use std::io::IsTerminal;
    let requested =
        audit || audit_review.audit_agent.is_some() || audit_review.audit_model.is_some();
    let enough = trust.audits(&candidate.release).len() >= require_attestations;
    let reuse_review = !requested
        && !allow_unaudited
        && !enough
        && trust.source_build_reviews(&candidate.release).len() >= require_attestations;
    if !from_source
        && !requested
        && !reuse_review
        && !allow_unaudited
        && !non_interactive
        && !json
        && std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
        && !enough
        && candidate.release.data.package.source.is_some()
    {
        audit = dialoguer::Confirm::new()
            .with_prompt("This release needs an audit. Audit and install?")
            .default(false)
            .interact()?;
        if !audit {
            trust.authorize_install(
                &candidate.release,
                allow_untrusted || package.contains('/'),
                require_attestations,
                allow_warnings,
            )?;
        }
    }
    let built = candidate.release.data.package.source.is_some()
        && (from_source || requested || audit || reuse_review);
    let candidate = if built {
        trust.authorize_with_policy(
            &candidate.release,
            allow_untrusted || package.contains('/'),
            0,
            allow_warnings,
        )?;
        ensure!(
            !from_source || require_attestations == 1,
            "a local source build provides one reviewer; more independent audits are required by this installation's policy"
        );
        let other_audits = trust
            .audits(&candidate.release)
            .into_iter()
            .filter(|event| Some(event.pubkey.to_hex()) != config.identity)
            .count();
        ensure!(
            other_audits + 1 >= require_attestations,
            "a self-audit adds one reviewer; more independent audits are required by this installation's policy"
        );
        let result = cli_source::from_release(
            home,
            config,
            trust,
            &candidate,
            &audit_review,
            recipe_file.as_deref(),
            non_interactive,
            cli_source::InstallMode {
                prefer_published: !from_source,
                reuse_review,
                allow_source_fallback: require_attestations == 1,
            },
        )
        .await;
        match result {
            Ok(candidate) => candidate,
            Err(error) => {
                if let Err(publication_error) = cli_audit::offer_warning(
                    home,
                    config,
                    trust,
                    &candidate.release,
                    &error,
                    !non_interactive
                        && !json
                        && std::io::stdin().is_terminal()
                        && std::io::stderr().is_terminal(),
                )
                .await
                {
                    eprintln!("Warning publication did not complete: {publication_error:#}");
                }
                return Err(error);
            }
        }
    } else {
        candidate
    };
    let completed_review = AuditReviewArgs::default();
    ensure_audited(
        home,
        config,
        trust,
        &candidate,
        AuditOptions {
            requested: !built && requested,
            review: if built {
                &completed_review
            } else {
                &audit_review
            },
            non_interactive,
            allow_untrusted: allow_untrusted || package.contains('/'),
            allow_warnings,
            allow_unaudited,
            minimum: require_attestations,
        },
    )
    .await?;
    if !trust.socially_trusted(&candidate.release) {
        eprintln!(
            "Warning: {} is not authored or vouched for by your social graph (or its author/vouchers are overmuted); proceeding with your explicit choice.",
            release_label(&candidate.release, &config.aliases)
        );
    }
    if !json {
        print_install_start(&candidate.release, trust, &config.aliases);
    }
    cli_dependencies::install(
        home,
        config,
        trust,
        installation,
        &candidate,
        AuditOptions {
            requested: audit && !from_source,
            review: if from_source {
                &completed_review
            } else {
                &audit_review
            },
            non_interactive,
            allow_untrusted: allow_untrusted || package.contains('/'),
            allow_warnings,
            allow_unaudited,
            minimum: require_attestations,
        },
        allow_untrusted,
        &pinned_candidates,
    )
    .await?;
    progress.clear();
    print_installed(&candidate.release, trust, &config.aliases, json)?;
    Ok(())
}
