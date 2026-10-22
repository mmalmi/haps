//! Explicit source builds. Git transport (including htree) belongs to Git and
//! its installed remote helpers. Recipes run only after `--execute` is supplied.
mod artifacts;
pub mod manifest;

use crate::model::{MAX_METADATA, PackageSpec, SourceInfo, safe_path, target};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub package: PackageSpec,
    pub build: Steps,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Steps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// Commands are portable; makepkg is an optional builder on Arch Linux.
    #[serde(default)]
    pub builder: Builder,
    #[serde(default)]
    pub commands: Vec<Vec<String>>,
    /// Destination inside the package -> path inside the checkout.
    #[serde(default)]
    pub artifacts: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Builder {
    #[default]
    Commands,
    Makepkg,
}

pub struct SourceCheckout {
    directory: tempfile::TempDir,
    pub source: SourceInfo,
}

pub struct Checkout {
    source: SourceCheckout,
    pub recipe: Recipe,
}

fn git() -> Command {
    let mut git = Command::new("git");
    // A preview must not execute globally configured checkout filters/hooks.
    git.env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args([
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "protocol.file.allow=always",
        ]);
    git
}

fn git_ok(command: &mut Command) -> Result<String> {
    let result = command
        .output()
        .context("Git is required; htree URLs also require git-remote-htree on PATH")?;
    ensure!(
        result.status.success(),
        "Git failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(String::from_utf8(result.stdout)?.trim().to_string())
}

fn expand(value: &str) -> String {
    value
        .replace("{exe}", std::env::consts::EXE_SUFFIX)
        .replace("{target}", target())
}

impl SourceCheckout {
    pub fn fetch(source: SourceInfo, work: &Path) -> Result<Self> {
        source.validate()?;
        fs::create_dir_all(work)?;
        let directory = tempfile::Builder::new()
            .prefix("source-")
            .tempdir_in(work)?;
        let root = directory.path().join("repo");
        let mut clone = git();
        if source.git.starts_with("htree://") {
            crate::helpers::configure_git(&mut clone)?;
        }
        git_ok(
            clone
                .current_dir(directory.path())
                .args(["clone", "--no-checkout", "--", &source.git])
                .arg("repo"),
        )?;
        let commit = git_ok(git().current_dir(&root).args([
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", source.rev),
        ]))?;
        ensure!(
            commit.eq_ignore_ascii_case(&source.rev),
            "source did not resolve to the requested commit"
        );
        git_ok(
            git()
                .current_dir(&root)
                .args(["checkout", "--detach", &commit]),
        )?;
        Ok(Self { directory, source })
    }

    pub fn root(&self) -> PathBuf {
        self.directory.path().join("repo")
    }

    pub fn keep(self) -> PathBuf {
        self.directory.keep()
    }

    pub fn recipe(&self, requested: Option<&str>) -> Result<Option<Recipe>> {
        self.recipe_for(requested, None)
    }

    pub fn recipe_for(
        &self,
        requested: Option<&str>,
        package: Option<&str>,
    ) -> Result<Option<Recipe>> {
        let root = self.root();
        let names =
            requested.map_or_else(|| vec!["haps.toml", "haps-build.toml"], |name| vec![name]);
        for name in names {
            let relative = safe_path(name)?;
            let path = root.join(relative);
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                result => {
                    result?;
                }
            }
            let path = path.canonicalize()?;
            ensure!(
                path.starts_with(root.canonicalize()?),
                "recipe escapes checkout"
            );
            ensure!(
                fs::metadata(&path)?.len() <= MAX_METADATA as u64,
                "build recipe is too large"
            );
            let document =
                manifest::Manifest::select(&fs::read_to_string(path)?, target(), package)?;
            if let Some(build) = document.build {
                return Ok(Some(Recipe {
                    package: document.package,
                    build,
                }));
            }
        }
        Ok(None)
    }

    pub fn ensure_pristine(&self) -> Result<()> {
        let root = self.root();
        let status = git_ok(git().current_dir(&root).args([
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored",
        ]))?;
        ensure!(
            status.is_empty(),
            "source changed during the audit; start a new audit"
        );
        let head = git_ok(git().current_dir(root).args(["rev-parse", "HEAD"]))?;
        ensure!(
            self.source.rev.eq_ignore_ascii_case(&head),
            "source revision changed during audit"
        );
        Ok(())
    }
}

impl Checkout {
    pub fn project(source: SourceInfo, work: &Path) -> Result<Self> {
        Self::project_for(source, work, None)
    }

    pub fn project_for(source: SourceInfo, work: &Path, package: Option<&str>) -> Result<Self> {
        let source = SourceCheckout::fetch(source, work)?;
        let recipe = source.recipe_for(None, package)?.context("no build recipe declared in haps.toml or haps-build.toml; use --from-source to build from reviewed project instructions. A source review alone cannot verify the downloaded binary")?;
        Self::with_recipe(source, recipe)
    }
    pub fn fetch(source: SourceInfo, recipe_path: &str, work: &Path) -> Result<Self> {
        let source = SourceCheckout::fetch(source, work)?;
        let recipe = source.recipe(Some(recipe_path))?.with_context(|| format!(
            "build recipe {recipe_path} is missing at the pinned source revision. Record a completed manual audit with explicit provenance if the source-to-binary relationship is unverified"
        ))?;
        Self::with_recipe(source, recipe)
    }

    pub fn with_recipe(source: SourceCheckout, mut recipe: Recipe) -> Result<Self> {
        if let Some(reason) = &recipe.build.unavailable {
            ensure!(
                !reason.trim().is_empty() && reason.len() <= 1024,
                "invalid source build availability reason"
            );
            anyhow::bail!("source build unavailable: {}", reason.escape_debug());
        }
        if recipe.package.target == "host" {
            recipe.package.target = target().into();
        }
        recipe.package.source = Some(source.source.clone());
        recipe.package.commands = recipe
            .package
            .commands
            .into_iter()
            .map(|(k, v)| (k, expand(&v)))
            .collect();
        recipe.build.commands = recipe
            .build
            .commands
            .into_iter()
            .map(|args| args.into_iter().map(|v| expand(&v)).collect())
            .collect();
        recipe.build.artifacts = recipe
            .build
            .artifacts
            .into_iter()
            .map(|(k, v)| (expand(&k), expand(&v)))
            .collect();
        recipe.package.validate()?;
        ensure!(
            recipe.package.target == target(),
            "build recipe targets another machine"
        );
        match recipe.build.builder {
            Builder::Commands => ensure!(
                !recipe.build.commands.is_empty() && recipe.build.commands.len() <= 100,
                "recipe needs 1–100 build commands"
            ),
            Builder::Makepkg => {
                ensure!(
                    recipe.build.commands.is_empty(),
                    "makepkg recipes use PKGBUILD; omit build.commands"
                );
                ensure!(
                    recipe.package.target.contains("-linux-"),
                    "makepkg requires a Linux target; use ordinary build commands on other platforms"
                );
                let path = source
                    .root()
                    .join("PKGBUILD")
                    .canonicalize()
                    .context("makepkg needs PKGBUILD in the pinned source")?;
                ensure!(
                    path.starts_with(source.root().canonicalize()?) && path.is_file(),
                    "PKGBUILD must be a file inside the pinned source"
                );
            }
        }
        ensure!(
            !recipe.build.artifacts.is_empty() && recipe.build.artifacts.len() <= 1000,
            "recipe needs 1–1000 artifacts"
        );
        for command in &recipe.build.commands {
            ensure!(
                !command.is_empty() && !command[0].is_empty() && command.len() <= 100,
                "invalid build command"
            );
        }
        for (destination, input) in &recipe.build.artifacts {
            if destination != "." {
                safe_path(destination)?;
            }
            safe_path(input)?;
        }
        Ok(Self { source, recipe })
    }

    pub fn root(&self) -> PathBuf {
        self.source.root()
    }

    /// Retain an explicitly prepared checkout for a separate review session.
    pub fn keep(self) -> PathBuf {
        self.source.keep()
    }

    /// Review must not change the source or introduce untracked build inputs.
    pub fn ensure_pristine(&self) -> Result<()> {
        self.source.ensure_pristine()
    }

    pub fn execute(&self) -> Result<PathBuf> {
        let root = self.source.root();
        self.ensure_pristine()?;
        if self.recipe.build.builder == Builder::Makepkg {
            let status = Command::new("makepkg")
                .args(["--holdver", "--log", "--noarchive"])
                .current_dir(&root)
                .stdout(std::io::stderr())
                .status()
                .context("makepkg is required for this recipe; install the Arch build tools and declared dependencies first")?;
            ensure!(
                status.success(),
                "makepkg failed; check its output for missing dependencies or build errors"
            );
        }
        for args in &self.recipe.build.commands {
            eprintln!("Building: {}", args.join(" "));
            let status = Command::new(&args[0])
                .args(&args[1..])
                .current_dir(&root)
                .stdout(std::io::stderr())
                .status()?;
            ensure!(status.success(), "build command failed: {}", args[0]);
        }
        let payload = self.source.directory.path().join("payload");
        artifacts::copy(&root, &payload, &self.recipe.build.artifacts)?;
        Ok(payload)
    }
}
