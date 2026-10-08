//! Release audit evidence. A review and a reproducible-build check are distinct.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{path::Path, process::Command};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reviewer {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_model: Option<String>,
}

#[derive(Deserialize)]
pub struct Review {
    verdict: String,
    pub note: String,
    pub reviewer: Reviewer,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub schema: String,
    pub method: String,
    pub provenance: String,
    pub binary_match: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<Reviewer>,
}

fn runner() -> Command {
    let mut command =
        Command::new(std::env::var_os("HAPS_PYTHON").unwrap_or_else(|| "python3".into()));
    command.arg("-c").arg(include_str!("integrations/audit.py"));
    command
}

pub fn agents() -> Result<Vec<String>> {
    let output = runner()
        .arg("agents")
        .output()
        .context("agent detection needs Python 3")?;
    ensure!(output.status.success(), "agent detection failed");
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// A user-selected agent reviews a bounded source snapshot without executing it.
/// Manual reviewers inspect the checkout before confirming in the terminal.
pub fn review(
    checkout: &crate::build::Checkout,
    agent: &str,
    model: Option<&str>,
    interactive: bool,
) -> Result<Review> {
    let root = checkout.root();
    eprintln!("Audit source: {}", root.display());
    eprintln!("Build recipe: {}", serde_json::to_string(&checkout.recipe)?);
    let review = if agent == "manual" {
        ensure!(model.is_none(), "a model applies only to an agent review");
        ensure!(
            interactive,
            "manual audits need a terminal; use haps attest --audited to record a completed review"
        );
        let passed = dialoguer::Confirm::new()
            .with_prompt("Review this source and recipe. Mark the review passed and run the build with your user permissions?")
            .default(false).interact()?;
        ensure!(passed, "audit not approved; nothing installed");
        let note = dialoguer::Input::<String>::new()
            .with_prompt("What did you check?")
            .interact_text()?;
        Review {
            verdict: "pass".into(),
            note,
            reviewer: Reviewer {
                agent: "manual".into(),
                requested_model: None,
                reported_model: None,
            },
        }
    } else {
        ensure!(
            matches!(agent, "codex" | "claude"),
            "supported audit agents: codex, claude, manual"
        );
        eprintln!(
            "Reviewing with {agent}; source is sent to its configured service. A passing review permits the build recipe to run with your user permissions."
        );
        let input = tempfile::NamedTempFile::new()?;
        crate::model::atomic_write(input.path(), &serde_json::to_vec(&checkout.recipe)?)?;
        let output = runner()
            .arg("review")
            .arg(&root)
            .arg(input.path())
            .arg(agent)
            .arg(model.unwrap_or(""))
            .output()
            .context("could not start audit agent")?;
        ensure!(
            output.status.success(),
            "audit incomplete: {}",
            String::from_utf8_lossy(&output.stderr)
                .trim()
                .escape_debug()
        );
        let report: Review = serde_json::from_slice(&output.stdout)
            .context("agent returned an invalid audit report")?;
        ensure!(
            report.verdict == "pass",
            "audit did not pass: {}",
            report.note.escape_debug()
        );
        report
    };
    ensure!(
        !review.note.trim().is_empty() && review.note.len() <= 4096,
        "audit needs a review note of at most 4096 bytes"
    );
    checkout.ensure_pristine()?;
    Ok(review)
}

pub async fn review_and_rebuild(
    home: &Path,
    release: &crate::model::Release,
    repository: &crate::repository::Repository,
    agent: &str,
    model: Option<&str>,
    interactive: bool,
) -> Result<(Evidence, String)> {
    let source = release.data.package.source.clone().context("this release has no pinned source; Audit and install cannot reproduce it. A reviewer can record an explicit audit with provenance instead")?;
    let checkout = crate::build::Checkout::fetch(
        source.clone(),
        "haps-build.toml",
        &home.join("audits/work"),
    )?;
    ensure!(
        checkout.recipe.package == release.data.package,
        "source recipe does not describe the exact published package"
    );
    let manifest: crate::model::Manifest = repository.json(&release.data.manifest).await?;
    manifest.validate(&release.data.package)?;
    let review = review(&checkout, agent, model, interactive)?;
    let evidence = rebuild(home, release, &checkout, review.reviewer).await?;
    Ok((evidence, review.note))
}

async fn rebuild(
    home: &Path,
    release: &crate::model::Release,
    checkout: &crate::build::Checkout,
    reviewer: Reviewer,
) -> Result<Evidence> {
    if crate::security::enabled(home) {
        let report = crate::security::scan_source(
            home,
            release
                .data
                .package
                .source
                .as_ref()
                .context("source missing")?,
            &checkout.root(),
        )?;
        crate::security::bind_report(home, release, report)?;
    }
    eprintln!("Rebuilding the audited source...");
    let payload = checkout.execute()?;
    let rebuilt_repo =
        crate::repository::Repository::local(checkout.root().parent().unwrap().join("comparison"))?;
    let rebuilt = rebuilt_repo
        .pack_payload(&release.data.package, &payload)
        .await?;
    ensure!(
        rebuilt == release.data.manifest,
        "rebuilt payload does not match the published release (files, bytes, or executable metadata differ). No audit approval was recorded and nothing was installed"
    );
    eprintln!("Rebuilt payload matches the published release.");
    Ok(Evidence {
        schema: "haps.audit.v1".into(),
        method: format!("{}-review-and-rebuild", reviewer.agent),
        provenance: format!(
            "Reviewed pinned source {}; rebuilt with the committed recipe and local toolchain. Complete payload manifest matches {}. Dependencies and toolchain are not independently verified.",
            release
                .data
                .package
                .source
                .as_ref()
                .context("source missing")?
                .rev,
            release.data.manifest
        ),
        binary_match: true,
        reviewer: Some(reviewer),
    })
}

/// A persistent, private checkout. Preparation never executes the build recipe.
pub fn prepare(home: &Path, release: &crate::model::Release) -> Result<std::path::PathBuf> {
    let source = release.data.package.source.clone().context("this release has no pinned source; record an explicit completed audit with provenance instead")?;
    let checkout =
        crate::build::Checkout::fetch(source, "haps-build.toml", &home.join("audits/pending"))?;
    ensure!(
        checkout.recipe.package == release.data.package,
        "source recipe does not describe the exact published package"
    );
    let directory = checkout
        .root()
        .parent()
        .context("checkout parent missing")?
        .to_owned();
    crate::model::atomic_write(
        &directory.join("release.json"),
        &serde_json::to_vec_pretty(&release.event)?,
    )?;
    Ok(checkout.keep())
}

fn source_fingerprint(
    root: &Path,
) -> Result<std::collections::BTreeMap<std::path::PathBuf, String>> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut files = std::collections::BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            let relative = path.strip_prefix(root)?;
            if relative == Path::new(".git") {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            let value = if metadata.is_dir() {
                pending.push(path.clone());
                "directory".to_owned()
            } else if metadata.is_symlink() {
                format!("symlink:{:?}", std::fs::read_link(&path)?)
            } else {
                ensure!(metadata.is_file(), "unsupported file in prepared source");
                let mut file = std::fs::File::open(&path)?;
                let mut hash = Sha256::new();
                let mut buffer = [0_u8; 65536];
                loop {
                    let count = file.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    hash.update(&buffer[..count]);
                }
                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let executable = false;
                format!("file:{executable}:{}", hex::encode(hash.finalize()))
            };
            files.insert(relative.to_owned(), value);
            ensure!(files.len() <= 100_000, "too many source files");
        }
    }
    Ok(files)
}

/// Finish an external review by comparing to a fresh pinned checkout, so edited
/// Git configuration, hooks, or untracked files cannot become build inputs.
pub async fn finish(
    home: &Path,
    session: &str,
    note: &str,
    reviewer: Reviewer,
) -> Result<(crate::model::Release, Evidence)> {
    ensure!(
        !session.is_empty()
            && session.len() <= 100
            && session
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid audit session name"
    );
    ensure!(
        !note.trim().is_empty() && note.len() <= 4096,
        "audit needs a review note of at most 4096 bytes"
    );
    let directory = home.join("audits/pending").join(session);
    let release =
        crate::model::Release::verify(crate::model::read_json(&directory.join("release.json"))?)?;
    let source = release
        .data
        .package
        .source
        .clone()
        .context("pinned source missing")?;
    let checkout =
        crate::build::Checkout::fetch(source, "haps-build.toml", &home.join("audits/work"))?;
    ensure!(
        checkout.recipe.package == release.data.package,
        "source recipe does not describe the exact published package"
    );
    ensure!(
        source_fingerprint(&directory.join("repo"))? == source_fingerprint(&checkout.root())?,
        "prepared source has changed; start a new audit instead of approving different files"
    );
    let evidence = rebuild(home, &release, &checkout, reviewer).await?;
    Ok((release, evidence))
}

impl Evidence {
    pub fn manual(provenance: String) -> Result<Self> {
        let value = Self {
            schema: "haps.audit.v1".into(),
            method: "explicit-review".into(),
            provenance,
            binary_match: false,
            reviewer: None,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(reviewer) = &self.reviewer {
            for value in std::iter::once(&reviewer.agent)
                .chain(reviewer.requested_model.iter())
                .chain(reviewer.reported_model.iter())
            {
                ensure!(
                    !value.trim().is_empty()
                        && value.len() <= 200
                        && !value.chars().any(char::is_control),
                    "invalid audit agent or model"
                );
            }
        }
        ensure!(self.schema == "haps.audit.v1", "unsupported audit evidence");
        ensure!(
            !self.method.trim().is_empty() && self.method.len() <= 100,
            "audit method is required"
        );
        ensure!(
            !self.provenance.trim().is_empty() && self.provenance.len() <= 4096,
            "describe the audited artifact's provenance (including any unverified source-to-binary relationship)"
        );
        Ok(())
    }
}
