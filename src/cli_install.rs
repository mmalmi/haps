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
        mut from_source,
        recipe_file,
        version,
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

    progress.stage("Reading package catalogs");
    let choices = candidates(home, config, None, Some(&package)).await?;
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
    let require_attestations = require_attestations.max(1).max(
        installation
            .receipts()?
            .get(&candidate.release.identity())
            .map_or(0, |r| r.minimum_attestations),
    );
    use std::io::IsTerminal;
    if !from_source
        && !audit
        && !allow_unaudited
        && !non_interactive
        && !json
        && std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
        && trust.audits(&candidate.release).len() < require_attestations
        && candidate.release.data.package.source.is_some()
    {
        let choice = dialoguer::Select::new()
            .with_prompt("Audit and install: choose what to install")
            .items([
                "Build reviewed source",
                "Verify the published binary",
                "Cancel",
            ])
            .default(0)
            .interact()?;
        ensure!(choice != 2, "installation cancelled");
        from_source = choice == 0;
        audit = choice == 1;
    }
    let candidate = if from_source {
        trust.authorize_with_policy(
            &candidate.release,
            allow_untrusted || package.contains('/'),
            0,
            allow_warnings,
        )?;
        ensure!(
            require_attestations == 1,
            "a local source build provides one source review; this policy requires more independent reviews"
        );
        cli_source::from_release(
            home,
            config,
            trust,
            &candidate,
            &audit_review,
            recipe_file.as_deref(),
            non_interactive,
        )
        .await?
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
    )
    .await?;
    progress.clear();
    print_installed(&candidate.release, trust, &config.aliases, json)?;
    Ok(())
}
