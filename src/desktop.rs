//! Freedesktop launchers generated only from verified, declarative package metadata.
use crate::model::{Release, atomic_write};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

fn value(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
        .replace(' ', "\\s")
}

fn executable(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .context("desktop executable path must be UTF-8")?;
    ensure!(
        !text.contains('=') && !text.chars().any(char::is_control),
        "unsupported desktop executable path"
    );
    let mut quoted = String::from("\"");
    for ch in text.chars() {
        if "\\\"`$".contains(ch) {
            quoted.push('\\');
        }
        if ch == '%' {
            quoted.push('%');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    // Desktop string escaping precedes Exec argument unquoting.
    Ok(quoted.replace('\\', "\\\\"))
}

pub fn filename(home: &Path, release: &Release) -> String {
    let scope = hex::encode(Sha256::digest(home.as_os_str().as_encoded_bytes()));
    format!(
        "haps-{}-{}-{}.desktop",
        &scope[..16],
        release.author(),
        release.data.package.name
    )
}

pub fn render(release: &Release, directory: &Path) -> Result<Option<String>> {
    let spec = &release.data.package;
    let Some(desktop) = &spec.desktop else {
        return Ok(None);
    };
    let command = directory.join(&spec.commands[&desktop.command]);
    let icon = directory.join(&desktop.icon);
    let icon = icon.to_str().context("desktop icon path must be UTF-8")?;
    // GIO checks the first Exec token before expanding %% in paths. A fixed
    // env launcher avoids losing entries whose installation path contains %.
    // The package executable remains a single escaped argument; no shell runs.
    Ok(Some(format!(
        "[Desktop Entry]\nType=Application\nName={}\nExec=/usr/bin/env -- {}\nIcon={}\nTerminal=false\nCategories=Network;\nX-Haps-Publisher={}\nX-Haps-Release={}\n",
        value(&desktop.name),
        executable(&command)?,
        value(icon),
        release.author(),
        release.event.id
    )))
}

/// Change a single owned launcher and commit its receipt. Restore the launcher
/// if saving the receipt fails; refuse to overwrite user/foreign modifications.
pub fn transaction(
    path: &Path,
    old: Option<&str>,
    new: Option<&str>,
    commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if old.is_none() && new.is_none() {
        return commit();
    }
    let existing = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    ensure!(
        existing.is_none() || existing.as_deref() == old || existing.as_deref() == new,
        "desktop entry was modified outside Haps; preserve or move it before retrying: {}",
        path.display()
    );
    write(path, new)?;
    if let Err(error) = commit() {
        write(path, existing.as_deref())
            .context("receipt save failed and desktop restoration failed")?;
        return Err(error);
    }
    Ok(())
}

fn write(path: &Path, content: Option<&str>) -> Result<()> {
    if let Some(content) = content {
        atomic_write(path, content.as_bytes())?;
    } else if let Err(e) = fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exec_escaping_and_transaction_failure() -> Result<()> {
        assert_eq!(
            executable(Path::new("/tmp/my $app/100%/run"))?,
            "\"/tmp/my \\\\$app/100%%/run\""
        );
        assert!(executable(Path::new("/tmp/bad=path/run")).is_err());
        let tmp = tempfile::tempdir()?;
        let file = tmp.path().join("app.desktop");
        fs::write(&file, "old")?;
        assert!(
            transaction(&file, Some("old"), Some("new"), || anyhow::bail!(
                "disk failure"
            ))
            .is_err()
        );
        assert_eq!(fs::read_to_string(&file)?, "old");
        assert!(transaction(&file, Some("other"), Some("new"), || Ok(())).is_err());
        assert_eq!(fs::read_to_string(&file)?, "old");
        Ok(())
    }
}
