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

pub(crate) fn helper_identity(helper: &Path) -> Result<impl Serialize> {
    let metadata = fs::metadata(helper)?;
    Ok((
        helper.canonicalize()?,
        metadata.len(),
        metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos(),
    ))
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
