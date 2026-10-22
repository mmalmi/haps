pub(crate) mod environment;
mod graph;
mod payload;
pub use graph::InstallRequest;

use crate::{model::*, repository::Repository};
use anyhow::{Context, Result, bail, ensure};
use nostr::Event;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub current: Event,
    pub previous: Option<Event>,
    #[serde(default)]
    pub minimum_attestations: usize,
}

pub struct Installation {
    home: PathBuf,
    desktop_dir: Option<PathBuf>,
    progress: crate::progress::Progress,
}

impl Installation {
    pub fn new(home: PathBuf) -> Result<Self> {
        fs::create_dir_all(&home)?;
        let desktop_dir = if cfg!(target_os = "linux") {
            dirs::data_dir().map(|p| p.join("applications"))
        } else if cfg!(windows) {
            dirs::data_dir().map(|p| p.join("Microsoft/Windows/Start Menu/Programs"))
        } else {
            None
        };
        Ok(Self {
            home: home.canonicalize()?,
            desktop_dir,
            progress: crate::progress::Progress::default(),
        })
    }
    pub fn with_progress(mut self, progress: crate::progress::Progress) -> Self {
        self.progress = progress;
        self
    }
    /// Override desktop registration for isolated installations and tests.
    pub fn with_desktop_dir(mut self, directory: Option<PathBuf>) -> Self {
        self.desktop_dir = directory;
        self
    }
    fn save_change(
        &self,
        before: Option<&Event>,
        after: Option<&Event>,
        receipts: &BTreeMap<String, Receipt>,
    ) -> Result<()> {
        self.change(before, after, || self.save(receipts))
    }
    fn change(
        &self,
        before: Option<&Event>,
        after: Option<&Event>,
        commit: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let old = before.cloned().map(Release::verify).transpose()?;
        let new = after.cloned().map(Release::verify).transpose()?;
        let release = new.as_ref().or(old.as_ref()).context("missing release")?;
        if let Some(directory) = crate::links::bindings(&self.home)?.get(&release.identity()) {
            return crate::links::transaction(
                &self.home,
                directory,
                old.as_ref(),
                new.as_ref(),
                |r| self.version_dir(r),
                || self.desktop_change(before, after, commit),
            );
        }
        self.desktop_change(before, after, commit)
    }
    /// Put stable command launchers in an explicitly selected directory.
    pub fn link(&self, name: &str, directory: Option<&Path>) -> Result<PathBuf> {
        let _guard = lock(&self.home.join(".install.lock"))?;
        let release = Release::verify(self.receipt(name)?.current)?;
        ensure!(
            !release.data.package.commands.is_empty(),
            "package provides no commands"
        );
        let directory = directory
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.home.join("bin"));
        fs::create_dir_all(&directory)?;
        let directory = crate::links::shell_path(directory.canonicalize()?);
        let mut bindings = crate::links::bindings(&self.home)?;
        if let Some(old) = bindings.get(&release.identity()) {
            ensure!(
                old.directory() == directory,
                "package already linked in {}",
                old.directory().display()
            );
        }
        crate::links::transaction(
            &self.home,
            &crate::links::Binding::Directory(directory.clone()),
            None,
            Some(&release),
            |r| self.version_dir(r),
            || {
                bindings.insert(
                    release.identity(),
                    crate::links::Binding::Directory(directory.clone()),
                );
                atomic_write(
                    &self.home.join("linked.json"),
                    &serde_json::to_vec_pretty(&bindings)?,
                )
            },
        )?;
        Ok(directory)
    }
    /// Activate a verified package at the exact command owned by the installer.
    /// A failed activation retains the old bundle and restores its symlink.
    pub fn adopt_installer(
        &self,
        name: &str,
        installer: &crate::bootstrap::Installer,
    ) -> Result<()> {
        let _guard = lock(&self.home.join(".install.lock"))?;
        let release = Release::verify(self.receipt(name)?.current)?;
        let mut bindings = crate::links::bindings(&self.home)?;
        ensure!(
            bindings
                .get(&release.identity())
                .is_none_or(|binding| binding.directory() == installer.directory()),
            "Haps is already linked in a different directory"
        );
        let binding = installer.binding(&release)?;
        crate::links::adopt_installer(
            &self.home,
            installer,
            &binding,
            &release,
            |r| self.version_dir(r),
            || {
                bindings.insert(release.identity(), binding.clone());
                atomic_write(
                    &self.home.join("linked.json"),
                    &serde_json::to_vec_pretty(&bindings)?,
                )
            },
        )
    }
    #[cfg(all(test, unix))]
    fn save_desktop_change(
        &self,
        before: Option<&Event>,
        after: Option<&Event>,
        receipts: &BTreeMap<String, Receipt>,
    ) -> Result<()> {
        self.desktop_change(before, after, || self.save(receipts))
    }
    fn desktop_change(
        &self,
        before: Option<&Event>,
        after: Option<&Event>,
        commit: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let Some(directory) = &self.desktop_dir else {
            return commit();
        };
        let old = before.cloned().map(Release::verify).transpose()?;
        let new = after.cloned().map(Release::verify).transpose()?;
        #[cfg(windows)]
        return crate::desktop::windows::transaction(
            &self.home,
            directory,
            old.as_ref(),
            new.as_ref(),
            |r| self.version_dir(r),
            commit,
        );
        #[cfg(not(windows))]
        {
            let release = new
                .as_ref()
                .or(old.as_ref())
                .context("missing desktop release")?;
            let mut old_text = old
                .as_ref()
                .map(|r| self.render_desktop(r))
                .transpose()?
                .flatten();
            let new_text = new
                .as_ref()
                .map(|r| self.render_desktop(r))
                .transpose()?
                .flatten();
            let path = directory.join(crate::desktop::filename(&self.home, release));
            // Migrate only an exact launcher previously generated for this signed
            // release. User edits still fail the transaction's ownership check.
            if let Some(previous) = old.as_ref().or(new.as_ref()) {
                let existing = fs::read_to_string(&path).ok();
                for legacy in
                    crate::desktop::previous_entries(previous, &self.version_dir(previous))?
                        .into_iter()
                        .flatten()
                {
                    if existing.as_ref() == Some(&legacy) {
                        old_text = Some(legacy);
                        break;
                    }
                }
            }
            crate::desktop::transaction(&path, old_text.as_deref(), new_text.as_deref(), commit)
        }
    }
    #[cfg(not(windows))]
    fn render_desktop(&self, release: &Release) -> Result<Option<String>> {
        let environment = (!release.data.package.dependencies.is_empty())
            .then(|| environment::directory(&self.home, release));
        crate::desktop::render_with_environment(
            release,
            &self.version_dir(release),
            environment.as_deref(),
        )
    }
    pub fn receipts(&self) -> Result<BTreeMap<String, Receipt>> {
        let file = self.home.join("installed.json");
        if file.exists() {
            read_json(&file)
        } else {
            Ok(BTreeMap::new())
        }
    }
    fn save(&self, receipts: &BTreeMap<String, Receipt>) -> Result<()> {
        atomic_write(
            &self.home.join("installed.json"),
            &serde_json::to_vec_pretty(receipts)?,
        )
    }
    fn version_dir(&self, release: &Release) -> PathBuf {
        self.home
            .join("packages")
            .join(release.author())
            .join(&release.data.package.name)
            .join(release.event.id.to_hex())
    }
    fn find<'a>(
        receipts: &'a BTreeMap<String, Receipt>,
        name: &str,
    ) -> Result<(&'a String, &'a Receipt)> {
        let matches: Vec<_> = receipts
            .iter()
            .filter(|(id, _)| *id == name || id.rsplit('/').next() == Some(name))
            .collect();
        ensure!(
            matches.len() == 1,
            "package is missing or ambiguous; use its full publisher/name"
        );
        Ok(matches[0])
    }
    pub fn receipt(&self, name: &str) -> Result<Receipt> {
        Ok(Self::find(&self.receipts()?, name)?.1.clone())
    }
    pub fn path(&self, name: &str) -> Result<PathBuf> {
        Ok(self.version_dir(&Release::verify(self.receipt(name)?.current)?))
    }
    pub fn command(&self, name: &str, command: Option<&str>) -> Result<PathBuf> {
        let release = Release::verify(self.receipt(name)?.current)?;
        let commands = &release.data.package.commands;
        let path = if let Some(command) = command {
            commands
                .get(command)
                .context("command not provided by this package")?
        } else if let Some(desktop) = &release.data.package.desktop {
            &commands[&desktop.command]
        } else if commands.len() == 1 {
            commands.values().next().unwrap()
        } else {
            bail!("choose a command with --command; for GUI bundles use `haps path`");
        };
        Ok(self.version_dir(&release).join(safe_path(path)?))
    }
    pub async fn install(&self, repo: &Repository, release: &Release) -> Result<()> {
        self.install_with_policy(repo, release, 0).await
    }
    pub async fn install_with_policy(
        &self,
        repo: &Repository,
        release: &Release,
        minimum_attestations: usize,
    ) -> Result<()> {
        self.install_plan(&[InstallRequest {
            repository: repo,
            release,
            minimum_attestations,
        }])
        .await
    }
    pub fn rollback(&self, name: &str) -> Result<()> {
        self.rollback_dependencies(name, None)
    }
    /// Activate only the exact rollback closure the caller has authorized.
    pub fn rollback_checked(&self, name: &str, expected: &[Release]) -> Result<()> {
        self.rollback_dependencies(name, Some(expected))
    }
    pub fn remove(&self, name: &str) -> Result<()> {
        let _guard = lock(&self.home.join(".install.lock"))?;
        let mut receipts = self.receipts()?;
        let id = Self::find(&receipts, name)?.0.clone();
        let removed = receipts.remove(&id).unwrap();
        Self::validate_receipts(&receipts)?;
        self.save_change(Some(&removed.current), None, &receipts)
    }
}

pub fn ensure_public_key(value: &str) -> Result<String> {
    Ok(nostr::PublicKey::parse(value)?.to_hex())
}

pub fn load_keys(path: &Path) -> Result<nostr::Keys> {
    let key = fs::read_to_string(path)
        .context("signing key unavailable; run `haps identity init` or provide --key-file")?;
    nostr::Keys::parse(key.trim()).context("invalid secret key file")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn reinstall_migrates_only_an_owned_legacy_launcher() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let home = tmp.path().join("home with spaces");
        let entries = tmp.path().join("applications");
        let installation = Installation::new(home)?.with_desktop_dir(Some(entries.clone()));
        let payload = tmp.path().join("payload");
        fs::create_dir_all(payload.join("bin"))?;
        fs::write(payload.join("bin/hello"), b"hello")?;
        fs::write(payload.join("icon.svg"), b"<svg/>")?;
        let spec = PackageSpec {
            name: "hello".into(),
            version: "1.0.0".parse()?,
            target: target().into(),
            description: "Launcher migration fixture".into(),
            commands: BTreeMap::from([("hello".into(), "bin/hello".into())]),
            app: None,
            dependencies: Vec::new(),
            source: None,
            desktop: Some(DesktopEntry {
                uri_schemes: Vec::new(),
                name: "Iris Chat".into(),
                command: "hello".into(),
                icon: "icon.svg".into(),
            }),
        };
        let repository = Repository::local(tmp.path().join("repo"))?;
        let release = repository
            .publish(&nostr::Keys::generate(), spec, &payload)
            .await?;
        installation.install(&repository, &release).await?;
        let path = entries.join(crate::desktop::filename(&installation.home, &release));
        let new = fs::read_to_string(&path)?;
        for legacy in crate::desktop::previous_entries(&release, &installation.path("hello")?)?
            .into_iter()
            .flatten()
        {
            fs::write(&path, &legacy)?;
            installation.install(&repository, &release).await?;
            assert_eq!(fs::read_to_string(&path)?, new);
            let modified = format!("{legacy}# user's edit\n");
            fs::write(&path, &modified)?;
            assert!(installation.install(&repository, &release).await.is_err());
            assert_eq!(fs::read_to_string(&path)?, modified);
        }
        Ok(())
    }

    // Exercise Linux registration on macOS too, without installing a Linux binary.
    #[tokio::test]
    async fn registration_repairs_space_escaped_names_and_preserves_user_edits() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let entries = tmp.path().join("applications");
        let installation = Installation::new(tmp.path().join("home with spaces"))?
            .with_desktop_dir(Some(entries.clone()));
        let payload = tmp.path().join("payload");
        fs::create_dir(&payload)?;
        fs::write(payload.join("iris-chat"), b"fixture")?;
        fs::write(payload.join("Iris Chat.svg"), b"<svg/>")?;
        let repository = Repository::local(tmp.path().join("repo"))?;
        let release = repository
            .publish(
                &nostr::Keys::generate(),
                PackageSpec {
                    name: "iris-chat".into(),
                    version: "1.0.0".parse()?,
                    target: "x86_64-unknown-linux-gnu".into(),
                    description: "Menu name fixture".into(),
                    commands: BTreeMap::from([("iris-chat".into(), "iris-chat".into())]),
                    app: None,
                    dependencies: Vec::new(),
                    source: None,
                    desktop: Some(DesktopEntry {
                        uri_schemes: Vec::new(),
                        name: "Iris Chat".into(),
                        command: "iris-chat".into(),
                        icon: "Iris Chat.svg".into(),
                    }),
                },
                &payload,
            )
            .await?;
        let receipts = BTreeMap::from([(
            release.identity(),
            Receipt {
                current: release.event.clone(),
                previous: None,
                minimum_attestations: 0,
            },
        )]);
        installation.save_desktop_change(None, Some(&release.event), &receipts)?;
        let path = entries.join(crate::desktop::filename(&installation.home, &release));
        let expected = fs::read_to_string(&path)?;
        assert!(expected.contains("\nName=Iris Chat\n"));
        let icon = expected
            .lines()
            .find(|line| line.starts_with("Icon="))
            .unwrap();
        assert!(icon.ends_with("/Iris Chat.svg"));
        assert!(!icon.contains("\\s"));
        println!("Generated desktop entry:\n{expected}");
        let escaped: String = expected
            .lines()
            .map(|line| {
                let line = if line.starts_with("Name=") || line.starts_with("Icon=") {
                    line.replace(' ', "\\s")
                } else {
                    line.into()
                };
                format!("{line}\n")
            })
            .collect();
        assert_ne!(escaped, expected);
        let historical =
            crate::desktop::previous_entries(&release, &installation.version_dir(&release))?;
        assert_eq!(historical[0].as_ref(), Some(&escaped));
        assert!(
            historical[1]
                .as_ref()
                .unwrap()
                .contains("\nExec=/usr/bin/env -- ")
        );
        for legacy in historical.into_iter().flatten() {
            assert!(legacy.contains("\nName=Iris\\sChat\n"));
            fs::write(&path, &legacy)?;
            installation.save_desktop_change(
                Some(&release.event),
                Some(&release.event),
                &receipts,
            )?;
            assert_eq!(fs::read_to_string(&path)?, expected);
            for edited in [
                format!("{legacy}# user comment\n"),
                legacy.replace("Iris\\sChat", "My Chat"),
            ] {
                fs::write(&path, &edited)?;
                assert!(
                    installation
                        .save_desktop_change(Some(&release.event), Some(&release.event), &receipts)
                        .is_err()
                );
                assert_eq!(fs::read_to_string(&path)?, edited);
            }
        }
        Ok(())
    }
}
