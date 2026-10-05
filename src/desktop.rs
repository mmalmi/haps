//! Freedesktop launchers generated only from verified, declarative package metadata.
use crate::model::{Release, atomic_write};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

// Only this fixed script is evaluated. All package paths remain positional
// arguments, including quotes, dollar signs, and shell metacharacters.
const LINUX_LAUNCH_SCRIPT: &str = "XDG_DATA_DIRS=\"$1/usr/share:$1/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}\"; export XDG_DATA_DIRS; shift; exec \"$@\"";

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
    Ok(argument(text))
}

fn argument(text: &str) -> String {
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
    quoted.replace('\\', "\\\\")
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
    render_entry(release, directory, false)
}

/// Previous Haps versions launched without the package's resource directories.
pub(crate) fn render_legacy(release: &Release, directory: &Path) -> Result<Option<String>> {
    render_entry(release, directory, true)
}

fn render_entry(release: &Release, directory: &Path, legacy: bool) -> Result<Option<String>> {
    let spec = &release.data.package;
    let Some(desktop) = &spec.desktop else {
        return Ok(None);
    };
    let command = directory.join(&spec.commands[&desktop.command]);
    let icon = directory.join(&desktop.icon);
    let icon = icon.to_str().context("desktop icon path must be UTF-8")?;
    let exec = if legacy {
        format!("/usr/bin/env -- {}", executable(&command)?)
    } else {
        crate::launch::linux_data_dirs(directory, None)?;
        format!(
            "/bin/sh -c {} haps {} {}",
            argument(LINUX_LAUNCH_SCRIPT),
            executable(directory)?,
            executable(&command)?
        )
    };
    Ok(Some(format!(
        "[Desktop Entry]\nType=Application\nName={}\nExec={}\nIcon={}\nTerminal=false\nCategories=Network;\nX-Haps-Publisher={}\nX-Haps-Release={}\n",
        value(&desktop.name),
        exec,
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

    #[cfg(unix)]
    #[test]
    fn resource_launcher_preserves_arguments_and_data_dirs() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("app with $ and %f and \" quote");
        let program = tmp.path().join("program");
        fs::write(
            &program,
            "#!/bin/sh\nprintf '%s\\n' \"$XDG_DATA_DIRS\" \"$@\"\n",
        )?;
        for inherited in [None, Some(""), Some("/custom resources:/usr/share")] {
            let mut process = std::process::Command::new("/bin/sh");
            process
                .args(["-c", LINUX_LAUNCH_SCRIPT, "haps"])
                .arg(&root)
                .arg("/bin/sh")
                .arg(&program)
                .arg("literal $() `not a command` %f \" ;")
                .env_remove("XDG_DATA_DIRS");
            if let Some(value) = inherited {
                process.env("XDG_DATA_DIRS", value);
            }
            let output = process.output()?;
            assert!(output.status.success());
            let expected =
                crate::launch::linux_data_dirs(&root, inherited.map(std::ffi::OsStr::new))?;
            assert_eq!(
                String::from_utf8(output.stdout)?,
                format!(
                    "{}\nliteral $() `not a command` %f \" ;\n",
                    expected.to_string_lossy()
                )
            );
        }
        Ok(())
    }
}
