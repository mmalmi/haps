//! Discover optional executables independently of how Haps was installed.
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

pub fn find(name: &str) -> Option<PathBuf> {
    let executable = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            // Avoid implicitly trusting tools in the source checkout.
            if !directory.is_absolute() {
                continue;
            }
            let candidate = directory.join(&executable);
            if compatible(&candidate, name) {
                return Some(candidate);
            }
        }
    }
    let candidate = bundled(name)?;
    compatible(&candidate, name).then_some(candidate)
}

fn bundled(name: &str) -> Option<PathBuf> {
    Some(
        std::env::current_exe()
            .ok()?
            .canonicalize()
            .ok()?
            .parent()?
            .join("libexec")
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX)),
    )
}

fn compatible(path: &Path, name: &str) -> bool {
    if !path.is_file() {
        return false;
    }
    let mut command = Command::new(path);
    if name == "htree" {
        command.arg("--version");
    }
    let Ok(output) = command.output() else {
        return false;
    };
    if name == "htree" {
        let text = String::from_utf8_lossy(&output.stdout);
        output.status.success()
            && text
                .trim()
                .strip_prefix("htree ")
                .and_then(|v| semver::Version::parse(v).ok())
                .is_some_and(|v| v >= semver::Version::new(0, 2, 114))
    } else {
        String::from_utf8_lossy(&output.stderr).contains("Usage: git-remote-htree")
    }
}

pub fn configure_git(command: &mut Command) -> Result<()> {
    let Some(helper) = find("git-remote-htree") else {
        bail!(
            "Hashtree source builds need git-remote-htree. Install the Haps bundle or run `cargo install git-remote-htree --locked`."
        );
    };
    configure_git_helper(command, &helper)
}

/// Old Git helpers can announce a root without uploading its complete pack
/// contents. Check the writer before any source ref is published, independently
/// of the separate htree executable's version. Source reads remain compatible.
pub(crate) fn configure_git_publisher(command: &mut Command) -> Result<()> {
    let lock: serde_json::Value =
        serde_json::from_str(include_str!("../scripts/hashtree-bundle.json"))?;
    let hashes: Vec<_> = lock["assets"]
        .as_object()
        .context("invalid bundled helper pins")?
        .values()
        .filter_map(|asset| asset["git_remote_htree_sha256"].as_str().map(str::to_owned))
        .collect();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let candidates = std::env::split_paths(&path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| {
            directory.join(format!("git-remote-htree{}", std::env::consts::EXE_SUFFIX))
        });
    let helper = find_publisher(bundled("git-remote-htree"), candidates, &hashes).context(
        "Source publishing needs an updated Git helper. Install the current Haps bundle, or update git-remote-htree.",
    )?;
    configure_git_helper(command, &helper)
}

fn find_publisher(
    bundled: Option<PathBuf>,
    candidates: impl Iterator<Item = PathBuf>,
    hashes: &[String],
) -> Option<PathBuf> {
    bundled.into_iter().chain(candidates).find(|path| {
        if !path.is_file() {
            return false;
        }
        if helper_hash(path).is_some_and(|hash| hashes.contains(&hash)) {
            return true;
        }
        Command::new(path)
            .arg("--publisher-capabilities")
            .output()
            .is_ok_and(|output| output.status.success() && publisher_capabilities(&output.stdout))
    })
}

fn helper_hash(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > 128 * 1024 * 1024 {
        return None;
    }
    let mut digest = Sha256::new();
    let mut buffer = [0; 32 * 1024];
    let mut size = 0;
    loop {
        let length = file.read(&mut buffer).ok()?;
        if length == 0 {
            return Some(hex::encode(digest.finalize()));
        }
        size += length;
        if size > 128 * 1024 * 1024 {
            return None;
        }
        digest.update(&buffer[..length]);
    }
}

fn publisher_capabilities(bytes: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct Capabilities {
        schema: u64,
        helper: String,
        version: String,
        capabilities: Vec<String>,
    }
    if bytes.len() > 8192 {
        return false;
    }
    serde_json::from_slice::<Capabilities>(bytes).is_ok_and(|value| {
        value.schema == 1
            && value.helper == "git-remote-htree"
            && semver::Version::parse(&value.version).is_ok()
            && value
                .capabilities
                .iter()
                .any(|capability| capability == "complete-blob-dag-v1")
    })
}

fn configure_git_helper(command: &mut Command, helper: &Path) -> Result<()> {
    let mut paths = vec![
        helper
            .parent()
            .context("invalid Git helper path")?
            .to_path_buf(),
    ];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    command.env("PATH", std::env::join_paths(paths)?);
    // Existing Git helpers opt in to using the daemon. Preserve an explicit
    // user choice and otherwise match the shared Hashtree client's preference.
    if std::env::var_os("HTREE_PREFER_LOCAL_DAEMON").is_none() {
        command.env("HTREE_PREFER_LOCAL_DAEMON", "1");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_publisher_requires_the_complete_upload_contract() {
        let valid = serde_json::json!({
            "schema": 1, "helper": "git-remote-htree", "version": "0.2.93",
            "capabilities": ["future-capability", "complete-blob-dag-v1"]
        });
        assert!(publisher_capabilities(&serde_json::to_vec(&valid).unwrap()));
        for (field, value) in [
            ("schema", serde_json::json!(2)),
            ("schema", serde_json::json!("1")),
            ("helper", serde_json::json!("htree")),
            ("version", serde_json::json!("unknown")),
            ("capabilities", serde_json::json!(["push"])),
            (
                "capabilities",
                serde_json::json!(["complete-blob-dag-v1", 1]),
            ),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(!publisher_capabilities(
                &serde_json::to_vec(&invalid).unwrap()
            ));
        }
        assert!(!publisher_capabilities(b"Usage: git-remote-htree"));
    }

    #[test]
    fn a_qualified_bundle_precedes_path_and_changed_bytes_lose_qualification() -> Result<()> {
        let root = tempfile::tempdir()?;
        let bundled = root.path().join("bundled-helper");
        let system = root.path().join("system-helper");
        std::fs::write(&bundled, b"verified bundle bytes")?;
        std::fs::write(&system, b"separately verified bytes")?;
        let hashes = vec![
            helper_hash(&bundled).unwrap(),
            helper_hash(&system).unwrap(),
        ];
        let find = || {
            find_publisher(
                Some(bundled.clone()),
                std::iter::once(system.clone()),
                &hashes,
            )
        };
        assert_eq!(find(), Some(bundled.clone()));
        std::fs::write(&bundled, b"changed after installation")?;
        assert_eq!(find(), Some(system.clone()));
        std::fs::write(&system, b"unknown system helper")?;
        assert_eq!(find(), None);
        Ok(())
    }
}
