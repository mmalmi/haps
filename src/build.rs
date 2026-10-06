//! Explicit source builds. Git transport (including htree) belongs to Git and
//! its installed remote helpers. Recipes run only after `--execute` is supplied.
use crate::model::{MAX_METADATA, PackageSpec, SourceInfo, safe_path, target};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub package: PackageSpec,
    pub build: Steps,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Steps {
    pub commands: Vec<Vec<String>>,
    /// Destination inside the package -> path inside the checkout.
    pub artifacts: BTreeMap<String, String>,
}

pub struct Checkout {
    directory: tempfile::TempDir,
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

impl Checkout {
    pub fn fetch(source: SourceInfo, recipe_path: &str, work: &Path) -> Result<Self> {
        source.validate()?;
        let recipe_path = safe_path(recipe_path)?;
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
        let path = root
            .join(recipe_path)
            .canonicalize()
            .context("build recipe is missing")?;
        ensure!(
            path.starts_with(root.canonicalize()?),
            "recipe escapes checkout"
        );
        ensure!(
            fs::metadata(&path)?.len() <= MAX_METADATA as u64,
            "build recipe is too large"
        );
        let mut recipe: Recipe = toml::from_str(&fs::read_to_string(path)?)?;
        if recipe.package.target == "host" {
            recipe.package.target = target().into();
        }
        recipe.package.source = Some(source);
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
        ensure!(
            !recipe.build.commands.is_empty() && recipe.build.commands.len() <= 100,
            "recipe needs 1–100 build commands"
        );
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
            safe_path(destination)?;
            safe_path(input)?;
        }
        Ok(Self { directory, recipe })
    }

    pub fn execute(&self) -> Result<PathBuf> {
        let root = self.directory.path().join("repo");
        for args in &self.recipe.build.commands {
            eprintln!("Building: {}", args.join(" "));
            let status = Command::new(&args[0])
                .args(&args[1..])
                .current_dir(&root)
                .status()?;
            ensure!(status.success(), "build command failed: {}", args[0]);
        }
        let payload = self.directory.path().join("payload");
        fs::create_dir(&payload)?;
        let root = root.canonicalize()?;
        for (destination, input) in &self.recipe.build.artifacts {
            let source = root.join(safe_path(input)?).canonicalize()?;
            ensure!(
                source.starts_with(&root) && source.is_file(),
                "build artifact must be a file inside the checkout"
            );
            let output = payload.join(safe_path(destination)?);
            fs::create_dir_all(output.parent().unwrap())?;
            fs::copy(source, output)?;
        }
        Ok(payload)
    }
}
