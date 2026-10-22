//! Auditing has a separate poll frame from unrelated command handlers.
use crate::*;

pub(super) async fn run(
    home: &Path,
    config: &mut Config,
    trust: &mut Trust,
    action: AuditAction,
    non_interactive: bool,
) -> Result<()> {
    match action {
        AuditAction::Settings {
            agent,
            model,
            publish,
        } => {
            if let Some(agent) = agent {
                config.audit.agent = Some(audit_agent(Some(&agent), false)?);
            }
            if let Some(model) = model {
                ensure!(
                    !model.trim().is_empty()
                        && model.len() <= 200
                        && !model.chars().any(char::is_control),
                    "invalid audit model"
                );
                config.audit.model = (model != "default").then_some(model);
            }
            if publish.is_some() {
                config.audit.publish = publish;
            }
            save_config(home, config)?;
            println!("{}", serde_json::to_string_pretty(&config.audit)?);
        }
        AuditAction::Prepare { package, version } => {
            let package = resolve_package(config, &package)?;
            let choices = candidates(home, config, None, Some(&package)).await?;
            let releases = choices
                .iter()
                .map(|c| c.release.clone())
                .collect::<Vec<_>>();
            refresh_feedback(home, config, trust, &releases).await?;
            let candidate = select(
                choices,
                &package,
                version.as_ref(),
                &config.aliases,
                trust,
                target(),
                Selection {
                    non_interactive,
                    include_untrusted: package.contains('/'),
                    confirm_install: false,
                },
            )?;
            let directory = haps::audit::prepare(home, &candidate.release)?;
            println!("Source to review: {}", directory.join("repo").display());
            println!("Pinned release: {}", candidate.release.event.id);
            println!(
                "Preparation has not executed build commands. Review the source, project instructions, and optional haps.toml build section; keep the checkout unchanged."
            );
            println!("{}", haps::audit::REVIEW_SCOPE);
            println!(
                "After review, run with the same Haps home: haps audit finish {} --note \"What you checked\"",
                directory
                    .file_name()
                    .context("session name missing")?
                    .to_string_lossy()
            );
            println!(
                "With a recipe, finishing rebuilds and compares the published binary. Without one it records only a source review, which cannot approve a binary installation. Package: {} version {}.",
                release_label(&candidate.release, &config.aliases),
                candidate.release.data.package.version
            );
        }
        AuditAction::Finish {
            session,
            note,
            reviewer,
            model,
            publish,
            build_execution,
        } => {
            use std::io::IsTerminal;
            let interactive = !non_interactive
                && std::io::stdin().is_terminal()
                && std::io::stderr().is_terminal();
            let reviewer = haps::audit::Reviewer {
                agent: reviewer,
                requested_model: model,
                reported_model: None,
            };
            // Validate metadata and signing capability before running a build.
            let mut validation = haps::audit::Evidence::manual("External review".into())?;
            validation.reviewer = Some(reviewer.clone());
            validation.validate()?;
            let keys = publishing_keys(home, config)?;
            *trust = load_trust(home, config)?;
            let (release, evidence) =
                haps::audit::finish(home, &session, &note, reviewer, build_execution.execution())
                    .await?;
            record_audit(
                home,
                config,
                trust,
                &keys,
                &release,
                (&evidence, note),
                (publish, interactive),
            )
            .await?;
            println!(
                "Reviewed {} ({}): {}.",
                release_label(&release, &config.aliases),
                release.event.id,
                if evidence.binary_match {
                    "rebuilt binary matches; ready to install"
                } else {
                    "source reviewed; binary has not been verified"
                }
            );
        }
    }
    Ok(())
}
