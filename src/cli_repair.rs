//! Repair publication is explicit and limited to one freshly reviewed build.
use crate::*;
use haps::audit::repair::{self, Session};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    attempt: String,
    release: String,
}

pub(super) async fn run(
    home: &Path,
    config: &mut Config,
    trust: &mut Trust,
    action: RepairAction,
    non_interactive: bool,
) -> Result<()> {
    match action {
        RepairAction::Prepare { failed } => {
            let directory = repair::prepare(home, &failed)?;
            let session = directory
                .file_name()
                .context("repair session missing")?
                .to_string_lossy();
            println!("Repair session: {session}");
            println!("Editable source: {}", directory.join("repo").display());
            println!(
                "Original findings: {}",
                directory.join("audit-failure.json").display()
            );
            println!(
                "Original build recipe: {}",
                directory.join("original-recipe.toml").display()
            );
            println!(
                "Nothing built, installed, or published. Stop here if the package is not worth repairing."
            );
            println!(
                "If proceeding, fix the source, commit a newer package version and build instructions in haps.toml, and publish the fork source to your own repository. Keep the audited commit in its history."
            );
            println!(
                "Then run: haps audit repair build {session} --repository FORK_URL --rev FULL_COMMIT --note \"What was fixed\" --audit-agent codex"
            );
        }
        RepairAction::Build {
            session,
            repository,
            rev,
            note,
            audit_agent,
            audit_model,
            build_execution,
        } => {
            let session = Session::load(home, &session)?;
            let _guard = lock(&session.directory.join("repair.lock"))?;
            // A failed new attempt must not leave an older build selected for publication.
            // Its catalog and receipt remain in builds/ for inspection.
            atomic_write(&session.directory.join("ready.json"), b"null")?;
            let review = session.review_context(note)?;
            build_execution.execution().preflight()?;
            let source = haps::build::SourceCheckout::fetch(
                SourceInfo {
                    git: repository,
                    rev,
                },
                &home.join("audits/work"),
            )?;
            let recipe = source.recipe_for(None, Some(&session.failure.recipe.package.name))?
                .context("commit build instructions in the fork's haps.toml or haps-build.toml before rebuilding")?;
            let checkout = haps::build::Checkout::with_recipe(source, recipe)?;
            session.validate_fork(&checkout)?;
            let attempts = session.directory.join("builds");
            fs::create_dir_all(&attempts)?;
            let attempt = tempfile::Builder::new()
                .prefix("build-")
                .tempdir_in(&attempts)?
                .keep();
            let repair = cli_source::RepairBuild {
                catalog: attempt.join("catalog"),
                review,
            };
            let options = AuditReviewArgs {
                audit_agent,
                audit_model,
                publish_audit: Some(false),
                build_execution,
            };
            let candidate = cli_source::build_repair(
                home,
                config,
                trust,
                checkout,
                &options,
                non_interactive,
                &repair,
            )
            .await?;
            let ready = Ready {
                attempt: attempt
                    .file_name()
                    .context("build name missing")?
                    .to_string_lossy()
                    .into_owned(),
                release: candidate.release.event.id.to_hex(),
            };
            let bytes = serde_json::to_vec_pretty(&ready)?;
            atomic_write(&attempt.join("ready.json"), &bytes)?;
            atomic_write(&session.directory.join("ready.json"), &bytes)?;
            let name = session
                .directory
                .file_name()
                .context("session name missing")?
                .to_string_lossy();
            println!(
                "Reviewed and built {} {}",
                candidate.release.identity(),
                candidate.release.data.package.version
            );
            println!("Release: {}", candidate.release.event.id);
            println!(
                "Nothing installed or published. Test the repaired application before sharing."
            );
            println!(
                "Install locally: haps install {} --version {}",
                candidate.release.identity(),
                candidate.release.data.package.version
            );
            println!(
                "Publish explicitly: haps audit repair publish {name} --name YOUR_PUBLICATION_NAME"
            );
        }
        RepairAction::Publish { session, name } => {
            safe_name(&name)?;
            let session = Session::load(home, &session)?;
            let _guard = lock(&session.directory.join("repair.lock"))?;
            let ready: Option<Ready> = read_json(&session.directory.join("ready.json"))
                .context("no completed repair build; run haps audit repair build first")?;
            let ready = ready.context(
                "latest repair attempt did not complete; review and rebuild before publishing",
            )?;
            repair::session_name(&ready.attempt)?;
            let path = session
                .directory
                .join("builds")
                .join(&ready.attempt)
                .join("catalog");
            let keys = own_keys(home, config)?;
            let repo = Repository::local(path.clone())?;
            let snapshot = repo.catalog(&keys.public_key().to_hex()).await?;
            let releases = repo.releases(&snapshot).await?;
            ensure!(
                releases.len() == 1,
                "repair publication must contain exactly one reviewed release"
            );
            let release = &releases[0];
            ensure!(
                release.event.id.to_hex() == ready.release
                    && release.event.pubkey == keys.public_key(),
                "repair release differs from the completed build or current publisher"
            );
            repair::require_shareable(release)?;
            *trust = load_trust(home, config)?;
            let audit = trust.audits(release).into_iter()
                .find(|event| event.pubkey == keys.public_key())
                .context("repair no longer has a passing local audit; review and rebuild before publishing")?.clone();
            let evidence = haps::trust::parse_audit(&audit)?;
            ensure!(
                evidence
                    .build
                    .as_ref()
                    .is_some_and(|build| build.manifest == release.data.manifest),
                "repair audit does not cover this payload"
            );
            ensure!(
                trust.warnings(release).is_empty(),
                "repair has an active trusted warning; resolve it before publishing"
            );
            let mut events =
                haps::event_catalog::package_events(home, &path, &keys, Some(&name)).await?;
            events.push(audit);
            let discovery = Discovery::open(home)?;
            discovery.queue(&events)?;
            discovery.ingest(events.clone()).await?;
            flush_discovery(home, &discovery, &events).await?;
            println!(
                "Published repaired package {} {}",
                release.identity(),
                release.data.package.version
            );
            println!(
                "Readers still need their own trusted audit; your publisher self-review does not count for others."
            );
        }
    }
    Ok(())
}
