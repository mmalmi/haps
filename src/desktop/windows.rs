//! Per-user Windows integration. Package values are JSON data, never PowerShell.
use crate::model::Release;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command};

#[derive(Serialize)]
struct Entry {
    name: String,
    command: String,
    directory: String,
    icon: String,
    schemes: Vec<String>,
}
fn path(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .context("Windows desktop path must be UTF-8")?;
    ensure!(
        !value.contains('"') && !value.chars().any(char::is_control),
        "unsupported Windows desktop path"
    );
    // Windows canonical paths carry a verbatim prefix that Shell shortcuts do not support.
    if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        return Ok(format!(r"\\{unc}"));
    }
    Ok(value.strip_prefix(r"\\?\").unwrap_or(value).to_owned())
}
fn entry(release: &Release, directory: &Path) -> Result<Option<Entry>> {
    release
        .data
        .package
        .desktop
        .as_ref()
        .map(|desktop| {
            Ok(Entry {
                name: desktop.name.clone(),
                command: path(&directory.join(&release.data.package.commands[&desktop.command]))?,
                directory: path(directory)?,
                icon: path(&directory.join(&desktop.icon))?,
                schemes: desktop.uri_schemes.clone(),
            })
        })
        .transpose()
}

fn shortcut_path(shortcuts: &Path, owner: &str, name: &str) -> std::path::PathBuf {
    let scope = hex::encode(Sha256::digest(owner.as_bytes()));
    shortcuts
        .join(format!("Haps {}", &scope[..12]))
        .join(format!("{name}.lnk"))
}

pub(crate) fn transaction(
    home: &Path,
    shortcuts: &Path,
    old: Option<&Release>,
    new: Option<&Release>,
    directory: impl Fn(&Release) -> std::path::PathBuf,
    commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let release = new.or(old).context("missing desktop release")?;
    let old = old.map(|r| entry(r, &directory(r))).transpose()?.flatten();
    let new = new.map(|r| entry(r, &directory(r))).transpose()?.flatten();
    if old.is_none() && new.is_none() {
        return commit();
    }
    let owner = super::filename(home, release)
        .trim_end_matches(".desktop")
        .to_owned();
    let shortcut = path(&shortcut_path(
        shortcuts,
        &owner,
        &release.data.package.name,
    ))?;
    let temp = tempfile::Builder::new()
        .prefix("windows-desktop-")
        .tempdir_in(home)?;
    let script = temp.path().join("desktop.ps1");
    let plan = temp.path().join("plan.json");
    let journal = temp.path().join("journal.json");
    fs::write(&script, include_str!("windows.ps1"))?;
    fs::write(
        &plan,
        serde_json::to_vec(&serde_json::json!({
            "owner": owner, "shortcut": shortcut, "old": old, "new": new,
        }))?,
    )?;
    let run = |restore: bool| -> Result<()> {
        // Use the OS-provided executable, not one supplied by an installed package on PATH.
        let system = std::env::var_os("SystemRoot").context("SystemRoot is missing")?;
        let mut command =
            Command::new(Path::new(&system).join("System32/WindowsPowerShell/v1.0/powershell.exe"));
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script)
            .arg("-Plan")
            .arg(&plan)
            .arg("-Journal")
            .arg(&journal);
        if restore {
            command.arg("-Restore");
        }
        let result = command
            .output()
            .context("run Windows desktop integration")?;
        ensure!(
            result.status.success(),
            "Windows desktop integration failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(())
    };
    if let Err(error) = run(false) {
        // Retain recovery data if the OS rejected a change or restoration.
        let evidence = temp.keep();
        return Err(error.context(format!(
            "desktop recovery data retained at {}",
            evidence.display()
        )));
    }
    if let Err(error) = commit() {
        if let Err(restore) = run(true) {
            let evidence = temp.keep();
            return Err(error.context(format!(
                "desktop restoration failed: {restore}; recovery data: {}",
                evidence.display()
            )));
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::{
        model::{DesktopEntry, PackageSpec},
        repository::Repository,
    };
    use std::collections::BTreeMap;

    struct Cleanup {
        homes: Vec<std::path::PathBuf>,
        shortcuts: std::path::PathBuf,
        payload: std::path::PathBuf,
        releases: Vec<Release>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for home in &self.homes {
                for release in &self.releases {
                    let _ = transaction(
                        home,
                        &self.shortcuts,
                        Some(release),
                        None,
                        |_| self.payload.clone(),
                        || Ok(()),
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn native_shortcut_protocol_transaction_lifecycle() -> Result<()> {
        let root = tempfile::tempdir()?;
        let home = root.path().join("home with spaces");
        let shortcuts = root.path().join("shortcuts");
        let payload = root.path().join("app with spaces");
        fs::create_dir_all(&home)?;
        fs::create_dir_all(&payload)?;
        fs::write(payload.join("Hello.exe"), b"not executed")?;
        fs::write(payload.join("Hello.ico"), b"icon")?;
        let keys = nostr::Keys::generate();
        let scheme = format!("haps-test-{}", &keys.public_key().to_hex()[..20]);
        let mut spec = PackageSpec {
            name: "hello".into(),
            version: "1.0.0".parse()?,
            target: crate::model::target().into(),
            description: "Desktop transaction fixture".into(),
            commands: BTreeMap::from([("hello".into(), "Hello.exe".into())]),
            dependencies: vec![],
            source: None,
            app: None,
            desktop: Some(DesktopEntry {
                name: "Hello".into(),
                command: "hello".into(),
                icon: "Hello.ico".into(),
                uri_schemes: vec![scheme.clone()],
            }),
        };
        let repository = Repository::local(root.path().join("repo"))?;
        let first = repository.publish(&keys, spec.clone(), &payload).await?;
        spec.version = "2.0.0".parse()?;
        spec.desktop.as_mut().unwrap().name = "Hello v2".into();
        let second = repository.publish(&keys, spec, &payload).await?;
        let other = root.path().join("other");
        fs::create_dir_all(&other)?;
        let _cleanup = Cleanup {
            homes: vec![home.clone(), other.clone()],
            shortcuts: shortcuts.clone(),
            payload: payload.clone(),
            releases: vec![first.clone(), second.clone()],
        };
        let directory = |_: &Release| payload.clone();
        // A shortcut write failure after registration must undo the newly created handler.
        let blocked = root.path().join("not a directory");
        fs::write(&blocked, b"keep")?;
        assert!(transaction(&home, &blocked, None, Some(&first), directory, || Ok(())).is_err());
        assert_eq!(fs::read(&blocked)?, b"keep");
        transaction(&home, &shortcuts, None, Some(&first), directory, || Ok(()))?;
        let owner = super::super::filename(&home, &first).replace(".desktop", "");
        let link = shortcut_path(&shortcuts, &owner, &first.data.package.name);
        assert_eq!(link.file_name().unwrap(), "hello.lnk");
        let original = fs::read(&link)?;
        // Simulate an externally edited handler; Haps must preserve it unchanged.
        let fixture_script = root.path().join("foreign-handler.ps1");
        fs::write(
            &fixture_script,
            r#"param($Scheme, $Action, $Owner)
$ErrorActionPreference = 'Stop'
$key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey(('Software\Classes\' + $Scheme), $true)
try {
    if ($Action -eq 'mark') {
        if ($key.GetValue('HapsOwner') -cne $Owner) { throw 'unexpected owner' }
        $key.SetValue('HapsOwner', 'foreign-test-owner')
    } elseif ($Action -eq 'check') {
        if ($key.GetValue('HapsOwner') -cne 'foreign-test-owner') { throw 'foreign handler was changed' }
    } elseif ($Action -eq 'restore') {
        if ($key.GetValue('HapsOwner') -cne 'foreign-test-owner') { throw 'refusing to overwrite new owner' }
        $key.SetValue('HapsOwner', $Owner)
    }
} finally { $key.Dispose() }
"#,
        )?;
        let owner = super::super::filename(&home, &first).replace(".desktop", "");
        let fixture = |action: &str| -> Result<()> {
            let system = std::env::var_os("SystemRoot").context("missing SystemRoot")?;
            let result = Command::new(
                Path::new(&system).join("System32/WindowsPowerShell/v1.0/powershell.exe"),
            )
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&fixture_script)
            .arg(&scheme)
            .arg(action)
            .arg(&owner)
            .output()?;
            ensure!(
                result.status.success(),
                "foreign-handler fixture failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            Ok(())
        };
        fixture("mark")?;
        let rejected = transaction(
            &home,
            &shortcuts,
            Some(&first),
            Some(&second),
            directory,
            || Ok(()),
        )
        .is_err();
        let unchanged = fixture("check");
        // Restore fixture ownership before assertions, so cleanup remains safe on failure.
        fixture("restore")?;
        assert!(rejected);
        unchanged?;

        // Another HAPS_HOME cannot take the registered scheme, even with the same publisher.
        assert!(transaction(&other, &shortcuts, None, Some(&first), directory, || Ok(())).is_err());
        assert_eq!(fs::read(&link)?, original);
        assert!(
            transaction(
                &home,
                &shortcuts,
                Some(&first),
                Some(&second),
                directory,
                || anyhow::bail!("receipt write failed")
            )
            .is_err()
        );
        assert_eq!(fs::read(&link)?, original);
        transaction(
            &home,
            &shortcuts,
            Some(&first),
            Some(&second),
            directory,
            || Ok(()),
        )?;
        transaction(
            &home,
            &shortcuts,
            Some(&second),
            Some(&first),
            directory,
            || Ok(()),
        )?;
        transaction(&home, &shortcuts, Some(&first), None, directory, || Ok(()))?;
        assert!(!link.exists());
        // Removal frees the handler for a new Haps home rather than leaving a stale association.
        transaction(&other, &shortcuts, None, Some(&first), directory, || Ok(()))?;
        transaction(&other, &shortcuts, Some(&first), None, directory, || Ok(()))?;
        Ok(())
    }
}
