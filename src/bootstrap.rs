//! Recognize the installer's active bundle without trusting whichever copy is on PATH.
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub fn package() -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Mapping {
        publisher: String,
    }
    let mapping: Mapping = serde_json::from_str(include_str!("../haps-release.json"))?;
    Ok(format!(
        "{}/haps",
        nostr::PublicKey::parse(&mapping.publisher)?.to_hex()
    ))
}

#[derive(Debug, PartialEq, Eq)]
pub struct Installer {
    directory: PathBuf,
    executable: PathBuf,
    target: PathBuf,
}

impl Installer {
    pub fn current() -> Result<Option<Self>> {
        Self::from_executable(&std::env::current_exe()?)
    }

    pub fn from_executable(executable: &Path) -> Result<Option<Self>> {
        if !cfg!(unix) {
            return Ok(None);
        }
        let executable = executable.canonicalize()?;
        if executable.file_name().is_none_or(|name| name != "haps") {
            return Ok(None);
        }
        let Some(bundle) = executable.parent() else {
            return Ok(None);
        };
        let Some(bundles) = bundle.parent() else {
            return Ok(None);
        };
        if bundles.file_name().is_none_or(|name| name != ".haps") {
            return Ok(None);
        }
        let Some(directory) = bundles.parent() else {
            return Ok(None);
        };
        let target = Path::new(".haps")
            .join(bundle.file_name().unwrap())
            .join("haps");
        if fs::read_link(directory.join("haps")).ok().as_ref() != Some(&target) {
            return Ok(None);
        }
        Ok(Some(Self {
            directory: directory.to_path_buf(),
            executable,
            target,
        }))
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub(crate) fn owned_links(
        &self,
        release: &crate::model::Release,
    ) -> Result<BTreeMap<String, PathBuf>> {
        let mut links = BTreeMap::from([("haps".into(), self.target.clone())]);
        for name in ["htree", "git-remote-htree"] {
            if !release.data.package.commands.contains_key(name) {
                continue;
            }
            let expected = self.target.parent().unwrap().join("libexec").join(name);
            if fs::read_link(self.directory.join(name)).ok().as_ref() == Some(&expected) {
                links.insert(name.into(), expected);
            }
        }
        Ok(links)
    }

    pub(crate) fn binding(&self, release: &crate::model::Release) -> Result<crate::links::Binding> {
        self.binding_with_path(release, &std::env::var_os("PATH").unwrap_or_default())
    }

    fn binding_with_path(
        &self,
        release: &crate::model::Release,
        path: &std::ffi::OsStr,
    ) -> Result<crate::links::Binding> {
        self.verify()?;
        ensure!(
            release
                .data
                .package
                .commands
                .keys()
                .all(|name| matches!(name.as_str(), "haps" | "htree" | "git-remote-htree"))
                && release.data.package.commands.contains_key("haps"),
            "unexpected bootstrap commands"
        );
        let mut commands: BTreeSet<_> = self.owned_links(release)?.into_keys().collect();
        for name in ["htree", "git-remote-htree"] {
            if release.data.package.commands.contains_key(name)
                && fs::symlink_metadata(self.directory.join(name))
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                && !std::env::split_paths(path).any(|directory| {
                    if !directory.is_absolute() {
                        return false;
                    }
                    let Ok(metadata) = fs::metadata(directory.join(name)) else {
                        return false;
                    };
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    }
                    #[cfg(not(unix))]
                    metadata.is_file()
                })
            {
                commands.insert(name.into());
            }
        }
        Ok(crate::links::Binding::Selected {
            directory: self.directory.clone(),
            commands,
        })
    }

    pub fn verify(&self) -> Result<()> {
        ensure!(
            Self::from_executable(&self.executable)?.as_ref() == Some(self),
            "installer command changed during the update; leaving it unchanged"
        );
        Ok(())
    }

    pub fn validate_release(&self, release: &crate::model::Release) -> Result<()> {
        ensure!(
            release.identity() == package()?,
            "self-update publisher does not match the installer"
        );
        ensure!(
            release.data.package.version >= semver::Version::parse(env!("CARGO_PKG_VERSION"))?,
            "published Haps is older than this executable; refusing to downgrade"
        );
        ensure!(
            release
                .data
                .package
                .commands
                .get("haps")
                .map(String::as_str)
                == Some("haps")
                && release
                    .data
                    .package
                    .commands
                    .iter()
                    .all(|(name, path)| name == "haps"
                        || (matches!(name.as_str(), "htree" | "git-remote-htree")
                            && path == &format!("libexec/{name}"))),
            "Haps release does not contain the expected command"
        );
        self.verify()
    }
}

pub fn cargo_installation() -> Result<bool> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let root = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".cargo")))
        .context("cannot determine Cargo home")?;
    Ok(root
        .join("bin")
        .join(format!("haps{}", std::env::consts::EXE_SUFFIX))
        .canonicalize()
        .ok()
        .as_ref()
        == Some(&executable))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[tokio::test]
    async fn missing_helpers_are_selected_unless_a_foreign_command_exists() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let bin = temp.path().join("bin");
        let payload = bin.join(".haps/old");
        fs::create_dir_all(payload.join("libexec"))?;
        fs::write(payload.join("haps"), "fixture")?;
        symlink(".haps/old/haps", bin.join("haps"))?;
        let installer = Installer::from_executable(&payload.join("haps"))?.unwrap();
        for name in ["htree", "git-remote-htree"] {
            fs::write(payload.join("libexec").join(name), "fixture")?;
        }
        let release = crate::repository::Repository::local(temp.path().join("repo"))?
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
                    dependencies: Vec::new(),
                    source: None,
                },
                &payload,
            )
            .await?;
        let foreign = temp.path().join("foreign");
        fs::create_dir(&foreign)?;
        let path = std::env::join_paths([&foreign, &bin])?;
        let selected = || -> Result<BTreeSet<String>> {
            let crate::links::Binding::Selected { commands, .. } =
                installer.binding_with_path(&release, &path)?
            else {
                anyhow::bail!("expected selected commands");
            };
            Ok(commands)
        };
        assert_eq!(selected()?.len(), 3);
        fs::write(foreign.join("htree"), "not executable")?;
        fs::set_permissions(foreign.join("htree"), fs::Permissions::from_mode(0o644))?;
        assert_eq!(selected()?.len(), 3);
        fs::set_permissions(foreign.join("htree"), fs::Permissions::from_mode(0o755))?;
        assert_eq!(
            selected()?,
            BTreeSet::from(["haps".into(), "git-remote-htree".into()])
        );
        // Even a dangling link at the destination belongs to someone else.
        symlink("missing", bin.join("git-remote-htree"))?;
        assert_eq!(selected()?, BTreeSet::from(["haps".into()]));
        Ok(())
    }
}
