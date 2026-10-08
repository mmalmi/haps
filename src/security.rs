//! Opt-in source scans. Scan results are evidence, never automatic endorsements.
use crate::model::{Release, SourceInfo};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{path::Path, process::Command};

fn python() -> Command {
    let mut command =
        Command::new(std::env::var_os("HAPS_PYTHON").unwrap_or_else(|| "python3".into()));
    command
        .arg("-c")
        .arg(include_str!("integrations/security.py"));
    command
}

pub fn configure(
    home: &Path,
    rules: Option<&Path>,
    scanner: Option<&Path>,
    disable: bool,
) -> Result<()> {
    let mut command = python();
    command.arg("configure").arg(home);
    if disable {
        command.arg("disable");
    } else if let Some(rules) = rules {
        command
            .arg(rules)
            .arg(scanner.unwrap_or_else(|| Path::new("semgrep")));
    }
    ensure!(
        command.status()?.success(),
        "source scan configuration failed"
    );
    Ok(())
}

pub fn enabled(home: &Path) -> bool {
    home.join("security/policy.json").exists()
}

/// Scan before any build command or payload is executed. No source is uploaded.
pub fn scan_source(home: &Path, source: &SourceInfo, root: &Path) -> Result<Value> {
    let output = python()
        .arg("scan")
        .arg(home)
        .arg(root)
        .arg(&source.rev)
        .output()
        .context("source scans need Python 3.9 or newer and Semgrep")?;
    ensure!(
        output.status.success(),
        "source scan did not pass: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let report: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        report["result"] == "no_findings",
        "source scan did not pass"
    );
    eprintln!(
        "Source scan: no findings in {} files under the selected rules ({} files unscanned). This does not verify the binary was built from this source.",
        report["scanned_files"], report["unscanned_files"]
    );
    Ok(report)
}

pub fn bind_report(home: &Path, release: &Release, mut report: Value) -> Result<Value> {
    ensure!(
        release
            .data
            .package
            .source
            .as_ref()
            .is_some_and(|source| report["source_commit"] == source.rev),
        "source scan does not match the built release"
    );
    report["release"] = release.event.id.to_hex().into();
    report["manifest"] = release.data.manifest.clone().into();
    crate::model::atomic_write(
        &home
            .join("security/releases")
            .join(format!("{}.json", release.event.id)),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}

pub fn attestation(keys: &nostr::Keys, release: &Release, report: &Value) -> Result<nostr::Event> {
    ensure!(
        report["schema"] == "haps.source-scan.v1"
            && report["result"] == "no_findings"
            && report["release"] == release.event.id.to_hex()
            && report["manifest"] == release.data.manifest
            && release
                .data
                .package
                .source
                .as_ref()
                .is_some_and(|source| report["source_commit"] == source.rev)
            && report["scanned_files"]
                .as_u64()
                .is_some_and(|count| count > 0),
        "scan result does not match this release"
    );
    // A signed NIP-22 release comment is discoverable with existing feedback,
    // while remaining separate from positive attestations in the trust policy.
    crate::comments::create(
        keys,
        &release.event,
        true,
        None,
        serde_json::to_string(report)?,
    )
}
