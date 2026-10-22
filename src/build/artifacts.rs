use crate::model::{MAX_FILES, MAX_PACKAGE_BYTES, safe_path};
use anyhow::{Result, ensure};
use std::{collections::BTreeMap, fs, path::Path};

/// Stage declared outputs, including app bundles, without escaping the checkout.
pub(super) fn copy(
    root: &Path,
    payload: &Path,
    artifacts: &BTreeMap<String, String>,
) -> Result<()> {
    let root = root.canonicalize()?;
    fs::create_dir(payload)?;
    let mut pending = Vec::new();
    for (destination, input) in artifacts {
        let output = if destination == "." {
            payload.to_path_buf()
        } else {
            payload.join(safe_path(destination)?)
        };
        pending.push((root.join(safe_path(input)?), output, Vec::new()));
    }
    let (mut entries, mut bytes) = (0usize, 0u64);
    while let Some((source, destination, mut ancestors)) = pending.pop() {
        entries += 1;
        ensure!(entries <= MAX_FILES, "too many build artifacts");
        let source = source.canonicalize()?;
        ensure!(
            source.starts_with(&root),
            "build artifact escapes the checkout"
        );
        let metadata = fs::metadata(&source)?;
        if metadata.is_dir() {
            ensure!(
                !ancestors.contains(&source),
                "build artifact contains a directory cycle"
            );
            ancestors.push(source.clone());
            fs::create_dir_all(&destination)?;
            for entry in fs::read_dir(&source)? {
                let entry = entry?;
                pending.push((
                    entry.path(),
                    destination.join(entry.file_name()),
                    ancestors.clone(),
                ));
                ensure!(
                    pending.len() + entries <= MAX_FILES,
                    "too many build artifacts"
                );
            }
        } else {
            ensure!(destination != payload, "root artifact must be a directory");
            ensure!(
                metadata.is_file(),
                "build artifact must be a regular file or directory"
            );
            bytes = bytes
                .checked_add(metadata.len())
                .filter(|size| *size <= MAX_PACKAGE_BYTES)
                .ok_or_else(|| anyhow::anyhow!("build artifacts exceed the package size limit"))?;
            ensure!(
                !destination.exists(),
                "overlapping build artifact destinations"
            );
            fs::create_dir_all(destination.parent().unwrap())?;
            fs::copy(source, destination)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_app_directories_and_rejects_overlapping_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        fs::create_dir_all(root.join("App.app/Contents/MacOS")).unwrap();
        fs::write(root.join("App.app/Contents/MacOS/app"), "built app").unwrap();
        let artifacts = BTreeMap::from([("App.app".into(), "App.app".into())]);
        let payload = temp.path().join("payload");
        copy(&root, &payload, &artifacts).unwrap();
        assert_eq!(
            fs::read_to_string(payload.join("App.app/Contents/MacOS/app")).unwrap(),
            "built app"
        );
        let mut overlapping = artifacts;
        overlapping.insert(
            "App.app/Contents/MacOS/app".into(),
            "App.app/Contents/MacOS/app".into(),
        );
        assert!(copy(&root, &temp.path().join("overlapping"), &overlapping).is_err());
    }

    #[test]
    fn stages_directory_contents_at_root_without_allowing_escape_or_overlap() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        fs::create_dir_all(root.join("publish/resources")).unwrap();
        fs::write(root.join("publish/App.exe"), "built app").unwrap();
        fs::write(root.join("publish/resources/icon"), "icon").unwrap();
        let mut artifacts = BTreeMap::from([(".".into(), "publish".into())]);
        let payload = temp.path().join("payload");
        copy(&root, &payload, &artifacts).unwrap();
        assert!(payload.join("App.exe").is_file());
        assert!(payload.join("resources/icon").is_file());
        artifacts.insert("App.exe".into(), "publish/App.exe".into());
        assert!(copy(&root, &temp.path().join("overlap"), &artifacts).is_err());
        assert!(
            copy(
                &root,
                &temp.path().join("file"),
                &BTreeMap::from([(".".into(), "publish/App.exe".into())])
            )
            .is_err()
        );
        assert!(
            copy(
                &root,
                &temp.path().join("escape"),
                &BTreeMap::from([("../escape".into(), "publish".into())])
            )
            .is_err()
        );
    }

    #[test]
    #[cfg(unix)]
    fn internal_links_preserve_executables_but_escapes_and_cycles_fail() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(root.join("app/binary"), "built").unwrap();
        fs::set_permissions(root.join("app/binary"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("binary", root.join("app/alias")).unwrap();
        let artifacts = BTreeMap::from([("App.app".into(), "app".into())]);
        let payload = temp.path().join("good");
        copy(&root, &payload, &artifacts).unwrap();
        assert_eq!(
            fs::read_to_string(payload.join("App.app/alias")).unwrap(),
            "built"
        );
        assert_ne!(
            fs::metadata(payload.join("App.app/alias"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        symlink(".", root.join("app/loop")).unwrap();
        assert!(copy(&root, &temp.path().join("cycle"), &artifacts).is_err());
        fs::remove_file(root.join("app/loop")).unwrap();
        fs::write(temp.path().join("private"), "outside").unwrap();
        symlink(temp.path().join("private"), root.join("app/outside")).unwrap();
        assert!(copy(&root, &temp.path().join("escape"), &artifacts).is_err());
    }
}
