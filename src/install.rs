use crate::{model::*, repository::Repository};
use anyhow::{Context, Result, bail, ensure};
use futures::TryStreamExt;
use hashtree_core::Cid;
use nostr::Event;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
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
                || self.save_desktop_change(before, after, receipts),
            );
        }
        self.save_desktop_change(before, after, receipts)
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
        let directory = directory.canonicalize()?;
        let mut bindings = crate::links::bindings(&self.home)?;
        if let Some(old) = bindings.get(&release.identity()) {
            ensure!(
                old == &directory,
                "package already linked in {}",
                old.display()
            );
        }
        crate::links::transaction(
            &self.home,
            &directory,
            None,
            Some(&release),
            |r| self.version_dir(r),
            || {
                bindings.insert(release.identity(), directory.clone());
                atomic_write(
                    &self.home.join("linked.json"),
                    &serde_json::to_vec_pretty(&bindings)?,
                )
            },
        )?;
        Ok(directory)
    }
    fn save_desktop_change(
        &self,
        before: Option<&Event>,
        after: Option<&Event>,
        receipts: &BTreeMap<String, Receipt>,
    ) -> Result<()> {
        let Some(directory) = &self.desktop_dir else {
            return self.save(receipts);
        };
        let old = before.cloned().map(Release::verify).transpose()?;
        let new = after.cloned().map(Release::verify).transpose()?;
        let release = new
            .as_ref()
            .or(old.as_ref())
            .context("missing desktop release")?;
        let mut old_text = old
            .as_ref()
            .map(|r| crate::desktop::render(r, &self.version_dir(r)))
            .transpose()?
            .flatten();
        let new_text = new
            .as_ref()
            .map(|r| crate::desktop::render(r, &self.version_dir(r)))
            .transpose()?
            .flatten();
        let path = directory.join(crate::desktop::filename(&self.home, release));
        // Migrate only an exact launcher previously generated for this signed
        // release. User edits still fail the transaction's ownership check.
        if let Some(previous) = old.as_ref().or(new.as_ref()) {
            let existing = fs::read_to_string(&path).ok();
            for legacy in crate::desktop::previous_entries(previous, &self.version_dir(previous))?
                .into_iter()
                .flatten()
            {
                if existing.as_ref() == Some(&legacy) {
                    old_text = Some(legacy);
                    break;
                }
            }
        }
        crate::desktop::transaction(&path, old_text.as_deref(), new_text.as_deref(), || {
            self.save(receipts)
        })
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
        // Verify again at the installation boundary; caller-owned structs are not trusted.
        let release = Release::verify(release.event.clone())?;
        ensure!(
            release.data.package.target == target(),
            "release target does not match this machine ({})",
            target()
        );
        let _guard = lock_with_wait(&self.home.join(".install.lock"), || {
            self.progress
                .stage("Waiting for another package installation");
        })?;
        let mut receipts = self.receipts()?;
        let id = release.identity();
        let previous = receipts.get(&id).map(|r| r.current.clone());
        let minimum_attestations =
            minimum_attestations.max(receipts.get(&id).map_or(0, |r| r.minimum_attestations));
        if let Some(previous) = &previous {
            let old = Release::verify(previous.clone())?;
            if old.event.id == release.event.id {
                self.progress
                    .stage("Package is already current; checking registration");
                receipts.get_mut(&id).unwrap().minimum_attestations = minimum_attestations;
                self.save_change(Some(previous), Some(&release.event), &receipts)?;
                return Ok(());
            }
            ensure!(
                release.data.package.version > old.data.package.version,
                "refusing downgrade or changed release at the same version; use rollback for the previous installed release"
            );
        }
        self.progress.stage("Reading package manifest");
        let manifest: Manifest = repo.json(&release.data.manifest).await?;
        manifest.validate(&release.data.package)?;
        let final_dir = self.version_dir(&release);
        let parent = final_dir.parent().context("invalid install directory")?;
        fs::create_dir_all(parent)?;
        let stage = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(parent)?;
        let tree = repo.store.tree();
        self.progress.download(
            manifest.files.iter().map(|f| f.size).sum(),
            manifest.files.len(),
        );
        for file in &manifest.files {
            let output = stage.path().join(safe_path(&file.path)?);
            fs::create_dir_all(output.parent().unwrap())?;
            let mut destination = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)?;
            let cid = Cid::parse(&file.cid)?;
            let mut stream = tree.get_stream(&cid);
            let mut written = 0u64;
            while let Some(chunk) = stream.try_next().await? {
                written = written
                    .checked_add(chunk.len() as u64)
                    .context("file size overflow")?;
                ensure!(written <= file.size, "download exceeds signed file size");
                destination.write_all(&chunk)?;
                self.progress.advance(chunk.len() as u64, 0);
            }
            ensure!(written == file.size, "incomplete file: {}", file.path);
            // Check existence even for empty files: a missing root must not masquerade as empty content.
            use hashtree_core::Store;
            ensure!(repo.store.has(&cid.hash).await?, "file root is unavailable");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                destination.set_permissions(fs::Permissions::from_mode(if file.executable {
                    0o755
                } else {
                    0o644
                }))?;
            }
            destination.sync_all()?;
            self.progress.advance(0, 1);
        }
        self.progress.finish_download();
        self.progress.stage("Registering package");
        // A previous interrupted install may have left this immutable slot. Never
        // trust those bytes: replace it only after the fresh verified stage exists.
        if final_dir.exists() {
            fs::remove_dir_all(&final_dir)?;
        }
        fs::rename(stage.path(), &final_dir)?;
        receipts.insert(
            id,
            Receipt {
                current: release.event.clone(),
                previous: previous.clone(),
                minimum_attestations,
            },
        );
        self.save_change(previous.as_ref(), Some(&release.event), &receipts)?;
        Ok(())
    }
    pub fn rollback(&self, name: &str) -> Result<()> {
        let _guard = lock(&self.home.join(".install.lock"))?;
        let mut receipts = self.receipts()?;
        let (id, receipt) = Self::find(&receipts, name)?;
        let id = id.clone();
        let previous = receipt
            .previous
            .clone()
            .context("no previous version retained")?;
        let release = Release::verify(previous.clone())?;
        ensure!(
            self.version_dir(&release).is_dir(),
            "previous version is unavailable"
        );
        let current = receipt.current.clone();
        let minimum_attestations = receipt.minimum_attestations;
        receipts.insert(
            id,
            Receipt {
                current: previous.clone(),
                previous: Some(current.clone()),
                minimum_attestations,
            },
        );
        self.save_change(Some(&current), Some(&previous), &receipts)
    }
    pub fn remove(&self, name: &str) -> Result<()> {
        let _guard = lock(&self.home.join(".install.lock"))?;
        let mut receipts = self.receipts()?;
        let id = Self::find(&receipts, name)?.0.clone();
        let removed = receipts.remove(&id).unwrap();
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
            source: None,
            desktop: Some(DesktopEntry {
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
                    source: None,
                    desktop: Some(DesktopEntry {
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
