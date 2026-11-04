//! Private publication checkpoints; public package and Hashtree formats are unchanged.
use crate::model::{atomic_write, lock};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) fn supports(helper: &Path, command: &str, option: &str) -> bool {
    Command::new(helper)
        .args([command, "--help"])
        .output()
        .ok()
        .is_some_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .any(|word| word == option)
        })
}

pub(crate) fn scope(home: &Path, input: &impl Serialize) -> Result<PathBuf> {
    let config = hashtree_config::Config::load()?;
    let data = std::env::var_os("HTREE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| hashtree_config::get_hashtree_dir().join("data"));
    let key = serde_json::to_vec(&(
        input,
        config.blossom.all_write_servers(),
        canonical_path(&data)?,
        canonical_path(Path::new(&config.storage.data_dir))?,
    ))?;
    canonical_path(
        &home
            .join("publication")
            .join(hex::encode(Sha256::digest(key))),
    )
}

/// Resolve existing ancestors before creating private state, including symlinks.
fn canonical_path(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let mut ancestor = path.clone();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        missing.push(
            ancestor
                .file_name()
                .context("invalid publication path")?
                .to_owned(),
        );
        ancestor = ancestor
            .parent()
            .context("invalid publication path")?
            .to_path_buf();
    }
    let mut resolved = ancestor.canonicalize()?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

pub(crate) fn helper_identity(helper: &Path) -> Result<(PathBuf, String)> {
    let path = helper.canonicalize()?;
    let digest = helper_digest(&path)?;
    Ok((path, digest))
}

fn hash_helper(file: &mut fs::File) -> Result<String> {
    let mut digest = Sha256::new();
    std::io::copy(file, &mut digest)?;
    Ok(hex::encode(digest.finalize()))
}

#[cfg(not(unix))]
fn helper_digest(path: &Path) -> Result<String> {
    hash_helper(&mut fs::File::open(path)?)
}

#[cfg(unix)]
fn helper_digest(path: &Path) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    use std::sync::Mutex;
    use std::time::UNIX_EPOCH;

    // File metadata never enters the persistent scope. Cache one digest in this process
    // on systems that expose the inode's change time (including restored mtimes).
    type Stamp = (u64, u64, u64, i64, i64);
    static LAST: Mutex<Option<(Stamp, String)>> = Mutex::new(None);
    let stamp = |m: &fs::Metadata| (m.dev(), m.ino(), m.len(), m.ctime(), m.ctime_nsec());
    let mut file = fs::File::open(path)?;
    let meta = file.metadata()?;
    let original = stamp(&meta);
    // A coarse or freshly stamped file might still change within the same clock tick.
    let reusable = meta.ctime() > 0
        && meta.ctime_nsec() > 0
        && UNIX_EPOCH
            .elapsed()
            .is_ok_and(|now| now.as_secs().saturating_sub(meta.ctime() as u64) > 2);
    if reusable
        && let Some((cached, digest)) = &*LAST.lock().unwrap()
        && *cached == original
    {
        return Ok(digest.clone());
    }

    let digest = hash_helper(&mut file)?;
    ensure!(
        stamp(&fs::metadata(path)?) == original && stamp(&file.metadata()?) == original,
        "htree helper changed while hashing: {}",
        path.display()
    );
    if reusable {
        *LAST.lock().unwrap() = Some((original, digest.clone()));
    }
    Ok(digest)
}

#[derive(Serialize, Deserialize)]
struct Uploaded {
    root: String,
    checksum: String,
}

pub(crate) fn uploaded(scope: &Path) -> Option<String> {
    fs::read(scope.join("uploaded.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Uploaded>(&bytes).ok())
        .filter(|state| {
            crate::event_catalog::decode_root(&state.root).is_ok()
                && hex::encode(Sha256::digest(state.root.as_bytes())) == state.checksum
        })
        .map(|state| state.root)
}

pub(crate) fn remember(scope: &Path, root: &str) -> Result<()> {
    crate::event_catalog::decode_root(root)?;
    atomic_write(
        &scope.join("uploaded.json"),
        &serde_json::to_vec(&Uploaded {
            root: root.into(),
            checksum: hex::encode(Sha256::digest(root.as_bytes())),
        })?,
    )
}

pub(crate) fn upload_directory(home: &Path, helper: &Path, path: &Path) -> Result<String> {
    let scope = scope(home, &(path.canonicalize()?, helper_identity(helper)?))?;
    ensure!(
        !scope.starts_with(path.canonicalize()?),
        "publication cache must be outside the catalog"
    );
    let _guard = lock(&scope.join("publish.lock"))?;
    let mut command = Command::new(helper);
    command.arg("add").arg(path).arg("--local");
    if cfg!(unix) && supports(helper, "add", "--cache") {
        command.arg("--cache").arg(scope.join("files.json"));
    } else {
        eprintln!(
            "This htree helper does not support source-file caching; addressing the complete catalog. Update htree to enable incremental addressing."
        );
    }
    let output = command.output()?;
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    ensure!(
        output.status.success(),
        "Hashtree catalog publication failed"
    );
    let root = crate::event_catalog::uploaded_root(&output.stdout)?;
    let previous = uploaded(&scope);
    if previous.as_deref() == Some(&root) {
        return Ok(root);
    }
    let mut command = Command::new(helper);
    command.args(["push", root.strip_prefix("htree://").unwrap_or(&root)]);
    if let Some(previous) = previous {
        if supports(helper, "push", "--previous-root") {
            command.args([
                "--previous-root",
                previous.strip_prefix("htree://").unwrap_or(&previous),
            ]);
        } else {
            eprintln!(
                "This htree helper does not support retained-root delta uploads; checking the complete catalog. Update htree to enable incremental upload."
            );
        }
    }
    let output = command.output().context("cannot upload Hashtree catalog")?;
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    ensure!(
        output.status.success(),
        "Hashtree content upload failed; catalog was not announced"
    );
    remember(&scope, &root)?;
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn checkpoint_identity_follows_helper_bytes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("htree");
        let identity = || helper_identity(&path);
        let set_time = |seconds| {
            fs::OpenOptions::new()
                .write(true)
                .open(&path)?
                .set_modified(UNIX_EPOCH + Duration::from_secs(seconds))
        };

        fs::write(&path, b"helper A")?;
        set_time(1_600_000_000)?;
        let original = identity()?;
        set_time(1_700_000_000)?;
        assert_eq!(
            identity()?,
            original,
            "touching the same bytes keeps the scope"
        );

        fs::write(&path, b"helper B")?;
        set_time(1_600_000_000)?;
        assert_ne!(
            identity()?,
            original,
            "different bytes with the original length and timestamp need a fresh scope"
        );
        Ok(())
    }
}
