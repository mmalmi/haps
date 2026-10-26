//! Optional repair sessions preserve failed evidence outside the editable source.
use crate::{
    audit::{Finding, recipe_sha256},
    build::{Checkout, Recipe, SourceCheckout},
    model::{Release, SourceInfo, atomic_write, read_json},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
pub struct Failure {
    pub schema: String,
    pub error: String,
    pub recipe: Recipe,
    #[serde(default)]
    pub finding: Option<Finding>,
    #[serde(default)]
    pub release: Option<nostr::Event>,
}

#[derive(Serialize)]
pub struct ReviewContext {
    pub original_revision: String,
    pub original_recipe_sha256: String,
    pub original_release: Option<String>,
    pub prior_finding: String,
    pub summary: String,
}

impl ReviewContext {
    /// Only public identifiers and the explicitly supplied summary enter signed evidence.
    pub fn provenance(&self) -> String {
        format!(
            "Repair of source commit {} (recipe SHA-256 {}; upstream release {}). Repair summary: {}.",
            self.original_revision,
            self.original_recipe_sha256,
            self.original_release.as_deref().unwrap_or("not recorded"),
            self.summary
        )
    }
}

pub struct Session {
    pub directory: PathBuf,
    pub failure: Failure,
}

impl Session {
    pub fn load(home: &Path, name: &str) -> Result<Self> {
        session_name(name)?;
        let directory = home.join("audits/repairs").join(name);
        let failure = read_failure(&directory.join("audit-failure.json"))?;
        Ok(Self { directory, failure })
    }

    pub fn review_context(&self, summary: String) -> Result<ReviewContext> {
        ensure!(
            !summary.trim().is_empty()
                && summary.len() <= 1024
                && !summary.chars().any(char::is_control),
            "provide a public repair summary of 1–1024 bytes without control characters"
        );
        Ok(ReviewContext {
            original_revision: self.original_source()?.rev.clone(),
            original_recipe_sha256: recipe_sha256(&self.failure.recipe)?,
            original_release: self.failure.release.as_ref().map(|event| event.id.to_hex()),
            prior_finding: self.failure.finding.as_ref().map_or_else(
                || "The previous audit or build did not complete. Review all source and build inputs afresh; consult the retained local report separately.".into(),
                |finding| format!("{}: {}", finding.verdict, finding.note),
            ),
            summary,
        })
    }

    fn original_source(&self) -> Result<&SourceInfo> {
        self.failure
            .recipe
            .package
            .source
            .as_ref()
            .context("failed audit has no pinned source")
    }

    /// Check a freshly fetched, committed fork before any review or execution.
    pub fn validate_fork(&self, checkout: &Checkout) -> Result<()> {
        let original = self.original_source()?;
        let package = &checkout.recipe.package;
        let source = package
            .source
            .as_ref()
            .context("fork has no pinned source")?;
        ensure!(
            package.name == self.failure.recipe.package.name,
            "repair must keep the original package name"
        );
        ensure!(
            package.target == self.failure.recipe.package.target,
            "repair must keep the original target"
        );
        ensure!(
            package
                .version
                .cmp_precedence(&self.failure.recipe.package.version)
                .is_gt(),
            "repair needs a newer package version; commit a version bump in the fork manifest"
        );
        ensure!(
            !source.rev.eq_ignore_ascii_case(&original.rev),
            "repair must use a new committed revision"
        );
        let ancestor = crate::build::git()
            .current_dir(checkout.root())
            .args(["merge-base", "--is-ancestor", &original.rev, &source.rev])
            .output()?;
        ensure!(
            ancestor.status.success(),
            "fork must contain the audited commit in its history"
        );
        let diff = crate::build::git()
            .current_dir(checkout.root())
            .args([
                "diff",
                "--quiet",
                "--no-ext-diff",
                &original.rev,
                &source.rev,
                "--",
            ])
            .output()?;
        ensure!(
            diff.status.code() == Some(1),
            "repair must change the audited source tree"
        );
        checkout.ensure_pristine()
    }
}

pub fn session_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 100
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid repair session or build name"
    );
    Ok(())
}

fn read_failure(path: &Path) -> Result<Failure> {
    let failure: Failure = read_json(path)?;
    ensure!(
        failure.schema == "haps.failed-audit.v1",
        "unsupported failed audit report"
    );
    failure.recipe.package.validate()?;
    let source = failure
        .recipe
        .package
        .source
        .as_ref()
        .context("failed audit has no pinned source")?;
    source.validate()?;
    if let Some(finding) = &failure.finding {
        ensure!(
            finding.note.len() <= 4096 && finding.verdict.len() <= 100,
            "invalid retained finding"
        );
    }
    if let Some(event) = &failure.release {
        let release = Release::verify(event.clone())?;
        ensure!(
            release.data.package.source.as_ref() == Some(source),
            "failed report's source differs from its signed release"
        );
    }
    Ok(failure)
}

pub fn prepare(home: &Path, failed: &Path) -> Result<PathBuf> {
    let report = if failed.is_dir() {
        failed.join("audit-failure.json")
    } else {
        failed.to_owned()
    };
    let failure = read_failure(&report)?;
    let source = failure
        .recipe
        .package
        .source
        .clone()
        .context("failed audit has no pinned source")?;
    let checkout = SourceCheckout::fetch(source, &home.join("audits/repairs"))?;
    let directory = checkout
        .root()
        .parent()
        .context("checkout parent missing")?
        .to_owned();
    atomic_write(
        &directory.join("audit-failure.json"),
        &serde_json::to_vec_pretty(&failure)?,
    )?;
    atomic_write(
        &directory.join("original-recipe.toml"),
        toml::to_string_pretty(&failure.recipe)?.as_bytes(),
    )?;
    Ok(checkout.keep())
}

/// Local source URLs must never be advertised as a shareable repair.
pub fn require_shareable(release: &Release) -> Result<()> {
    let source = release
        .data
        .package
        .source
        .as_ref()
        .context("repair has no pinned source")?;
    source.validate()?;
    ensure!(
        source.git.starts_with("https://") || source.git.starts_with("htree://"),
        "publish the fork source first, then rebuild with its public https:// or htree:// URL and exact commit; local source paths cannot be published"
    );
    Ok(())
}
