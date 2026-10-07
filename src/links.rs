//! Explicit, owned launchers that follow package update and rollback receipts.
use crate::model::{Release, atomic_write, read_json};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub fn bindings(home: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let path = home.join("linked.json");
    if path.exists() {
        read_json(&path)
    } else {
        Ok(BTreeMap::new())
    }
}

fn scripts(
    home: &Path,
    release: Option<&Release>,
    version_dir: &impl Fn(&Release) -> PathBuf,
) -> Result<BTreeMap<String, String>> {
    let mut scripts = BTreeMap::new();
    if let Some(release) = release {
        for (name, path) in &release.data.package.commands {
            let executable = version_dir(release).join(path);
            let executable = executable.to_str().context("command path is not UTF-8")?;
            let home = home.to_str().context("home path is not UTF-8")?;
            ensure!(
                !executable.chars().chain(home.chars()).any(char::is_control),
                "invalid launcher path"
            );
            #[cfg(unix)]
            {
                let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
                scripts.insert(
                    name.clone(),
                    format!(
                        "#!/bin/sh\n# Managed by Haps\nHAPS_HOME={} exec {} \"$@\"\n",
                        quote(home),
                        quote(executable)
                    ),
                );
            }
            #[cfg(windows)]
            {
                ensure!(
                    !executable.contains('"') && !home.contains('"'),
                    "invalid launcher path"
                );
                scripts.insert(format!("{name}.cmd"), format!("@echo off\r\nsetlocal DisableDelayedExpansion\r\nset \"HAPS_HOME={}\"\r\n\"{}\" %*\r\n", home.replace('%', "%%"), executable.replace('%', "%%")));
            }
        }
    }
    Ok(scripts)
}

fn write(path: &Path, text: Option<&str>) -> Result<()> {
    if let Some(text) = text {
        atomic_write(path, text.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        }
    } else if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn transaction(
    home: &Path,
    directory: &Path,
    old: Option<&Release>,
    new: Option<&Release>,
    version_dir: impl Fn(&Release) -> PathBuf,
    commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let before = scripts(home, old, &version_dir)?;
    let after = scripts(home, new, &version_dir)?;
    let names: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    let mut changes = Vec::new();
    for name in names {
        let path = directory.join(name);
        // Never follow a foreign symlink or replace an unrelated command.
        if let Ok(meta) = fs::symlink_metadata(&path) {
            ensure!(
                meta.is_file(),
                "launcher is not an owned regular file: {}",
                path.display()
            );
        }
        let current = match fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        ensure!(
            current.is_none()
                || current.as_ref() == before.get(name)
                || current.as_ref() == after.get(name),
            "command was modified outside Haps: {}",
            path.display()
        );
        changes.push((path, current, after.get(name)));
    }
    let apply = || -> Result<()> {
        for (path, _, next) in &changes {
            write(path, next.map(String::as_str))?;
        }
        commit()
    };
    if let Err(error) = apply() {
        for (path, previous, _) in &changes {
            write(path, previous.as_deref()).context("failed to restore command launcher")?;
        }
        return Err(error);
    }
    Ok(())
}
