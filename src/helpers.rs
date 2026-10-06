//! Discover optional executables independently of how Haps was installed.
use anyhow::{Context, Result, bail};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub fn find(name: &str) -> Option<PathBuf> {
    let executable = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            // Avoid implicitly trusting tools in the source checkout.
            if !directory.is_absolute() {
                continue;
            }
            let candidate = directory.join(&executable);
            if compatible(&candidate, name) {
                return Some(candidate);
            }
        }
    }
    let candidate = std::env::current_exe()
        .ok()?
        .canonicalize()
        .ok()?
        .parent()?
        .join("libexec")
        .join(executable);
    compatible(&candidate, name).then_some(candidate)
}

fn compatible(path: &Path, name: &str) -> bool {
    if !path.is_file() {
        return false;
    }
    let mut command = Command::new(path);
    if name == "htree" {
        command.arg("--version");
    }
    let Ok(output) = command.output() else {
        return false;
    };
    if name == "htree" {
        let text = String::from_utf8_lossy(&output.stdout);
        output.status.success()
            && text
                .trim()
                .strip_prefix("htree ")
                .and_then(|v| semver::Version::parse(v).ok())
                .is_some_and(|v| v >= semver::Version::new(0, 2, 114))
    } else {
        String::from_utf8_lossy(&output.stderr).contains("Usage: git-remote-htree")
    }
}

pub fn configure_git(command: &mut Command) -> Result<()> {
    let Some(helper) = find("git-remote-htree") else {
        bail!(
            "Hashtree source builds need git-remote-htree. Install the Haps bundle or run `cargo install git-remote-htree --locked`."
        );
    };
    let mut paths = vec![
        helper
            .parent()
            .context("invalid Git helper path")?
            .to_path_buf(),
    ];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    command.env("PATH", std::env::join_paths(paths)?);
    // Existing Git helpers opt in to using the daemon. Preserve an explicit
    // user choice and otherwise match the shared Hashtree client's preference.
    if std::env::var_os("HTREE_PREFER_LOCAL_DAEMON").is_none() {
        command.env("HTREE_PREFER_LOCAL_DAEMON", "1");
    }
    Ok(())
}
