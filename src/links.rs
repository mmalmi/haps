//! Explicit, owned launchers that follow package update and rollback receipts.
use crate::model::{Release, read_json};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Binding {
    Directory(PathBuf),
    Selected {
        directory: PathBuf,
        commands: BTreeSet<String>,
    },
}

impl Binding {
    pub fn directory(&self) -> &Path {
        match self {
            Self::Directory(path)
            | Self::Selected {
                directory: path, ..
            } => path,
        }
    }
    fn commands(&self) -> Option<&BTreeSet<String>> {
        match self {
            Self::Directory(_) => None,
            Self::Selected { commands, .. } => Some(commands),
        }
    }
}

pub fn bindings(home: &Path) -> Result<BTreeMap<String, Binding>> {
    let path = home.join("linked.json");
    if path.exists() {
        read_json(&path)
    } else {
        Ok(BTreeMap::new())
    }
}

/// cmd.exe cannot execute batch files through canonical verbatim paths.
pub(crate) fn shell_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    if let Some(text) = path.to_str() {
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(disk) = text.strip_prefix(r"\\?\")
            && disk.as_bytes().get(1) == Some(&b':')
        {
            return PathBuf::from(disk);
        }
    }
    path
}

fn scripts(
    home: &Path,
    release: Option<&Release>,
    version_dir: &impl Fn(&Release) -> PathBuf,
    commands: Option<&BTreeSet<String>>,
) -> Result<BTreeMap<String, String>> {
    let mut scripts = BTreeMap::new();
    if let Some(release) = release {
        for (name, path) in &release.data.package.commands {
            if commands.is_some_and(|commands| !commands.contains(name)) {
                continue;
            }
            let executable = shell_path(version_dir(release).join(path));
            let executable = executable.to_str().context("command path is not UTF-8")?;
            let home = shell_path(home.to_path_buf());
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
        use std::io::Write;
        let mut stage =
            tempfile::NamedTempFile::new_in(path.parent().context("missing launcher directory")?)?;
        stage.write_all(text.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            stage
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        stage.as_file().sync_all()?;
        stage.persist(path).map_err(|e| e.error)?;
    } else if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn transaction(
    home: &Path,
    binding: &Binding,
    old: Option<&Release>,
    new: Option<&Release>,
    version_dir: impl Fn(&Release) -> PathBuf,
    commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    transaction_inner(home, binding, old, new, version_dir, None, commit)
}

pub(crate) fn adopt_installer(
    home: &Path,
    installer: &crate::bootstrap::Installer,
    binding: &Binding,
    release: &Release,
    version_dir: impl Fn(&Release) -> PathBuf,
    commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    installer.verify()?;
    let targets = installer.owned_links(release)?;
    transaction_inner(
        home,
        binding,
        None,
        Some(release),
        version_dir,
        Some(&targets),
        commit,
    )
}

enum Previous {
    Text(Option<String>),
    Symlink(PathBuf),
}

#[allow(clippy::too_many_arguments)]
fn transaction_inner(
    home: &Path,
    binding: &Binding,
    old: Option<&Release>,
    new: Option<&Release>,
    version_dir: impl Fn(&Release) -> PathBuf,
    installer_targets: Option<&BTreeMap<String, PathBuf>>,
    commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let directory = binding.directory();
    let before = scripts(home, old, &version_dir, binding.commands())?;
    let after = scripts(home, new, &version_dir, binding.commands())?;
    let names: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    let mut changes = Vec::new();
    for name in names {
        let path = directory.join(name);
        // Only the migration path admits the exact active installer symlink.
        if let Ok(meta) = fs::symlink_metadata(&path) {
            let installer_target = installer_targets.and_then(|targets| targets.get(name));
            if meta.is_symlink() && installer_target.is_some() {
                let target = fs::read_link(&path)?;
                ensure!(
                    Some(&target) == installer_target,
                    "installer command changed during the update"
                );
                changes.push((path, Previous::Symlink(target), after.get(name)));
                continue;
            }
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
        changes.push((path, Previous::Text(current), after.get(name)));
    }
    let apply = || -> Result<()> {
        for (path, _, next) in &changes {
            write(path, next.map(String::as_str))?;
        }
        commit()
    };
    if let Err(error) = apply() {
        for (path, previous, _) in &changes {
            match previous {
                Previous::Text(text) => write(path, text.as_deref())?,
                Previous::Symlink(target) => {
                    #[cfg(unix)]
                    {
                        let stage = tempfile::tempdir_in(directory)?;
                        let link = stage.path().join("haps");
                        std::os::unix::fs::symlink(target, &link)?;
                        fs::rename(link, path).context("failed to restore installer command")?;
                    }
                    #[cfg(not(unix))]
                    anyhow::bail!("unsupported installer symlink: {}", target.display());
                }
            }
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn failed_migration_commit_restores_all_original_symlinks() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let bin = temp.path().join("bin");
        let payload = temp.path().join("payload");
        fs::create_dir(&bin)?;
        fs::create_dir(&payload)?;
        fs::write(payload.join("haps"), "fixture")?;
        fs::create_dir(payload.join("libexec"))?;
        for name in ["htree", "git-remote-htree"] {
            fs::write(payload.join("libexec").join(name), "fixture")?;
        }
        let repo = crate::repository::Repository::local(temp.path().join("repo"))?;
        let release = repo
            .publish(
                &nostr::Keys::generate(),
                crate::model::PackageSpec {
                    name: "haps".into(),
                    version: "1.0.0".parse()?,
                    target: crate::model::target().into(),
                    description: "Fixture".into(),
                    commands: BTreeMap::from([
                        ("haps".into(), "haps".into()),
                        ("htree".into(), "libexec/htree".into()),
                        ("git-remote-htree".into(), "libexec/git-remote-htree".into()),
                    ]),
                    app: None,
                    desktop: None,
                    source: None,
                },
                &payload,
            )
            .await?;
        let targets = BTreeMap::from([
            ("haps".into(), PathBuf::from(".haps/old/haps")),
            ("htree".into(), PathBuf::from(".haps/old/libexec/htree")),
            (
                "git-remote-htree".into(),
                PathBuf::from(".haps/old/libexec/git-remote-htree"),
            ),
        ]);
        for (name, target) in &targets {
            std::os::unix::fs::symlink(target, bin.join(name))?;
        }
        let error = transaction_inner(
            temp.path(),
            &Binding::Directory(bin.clone()),
            None,
            Some(&release),
            |_| payload.clone(),
            Some(&targets),
            || anyhow::bail!("fixture commit failure"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("fixture commit failure"));
        for (name, target) in &targets {
            assert_eq!(fs::read_link(bin.join(name))?, *target);
        }
        Ok(())
    }
}
