//! Adapt the shared Hashtree/Iris Git release directory to signed Haps packages.
use crate::{install::load_keys, model::*, repository::Repository};
use anyhow::{Context, Result, ensure};
use clap::Args;
use nostr::Keys;
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Args)]
pub struct ReleaseArgs {
    /// Existing verified release directory containing release.json and assets.
    pub directory: PathBuf,
    /// Declarative package-to-asset mappings checked into the releasing project.
    #[arg(long)]
    pub config: PathBuf,
    /// Explicit immutable release tag; never selects latest implicitly.
    #[arg(long)]
    pub tag: String,
    #[arg(long, env = "HAPS_KEY_FILE")]
    pub key_file: Option<PathBuf>,
    /// Validate signer, checksums and package layouts without signing or publishing.
    #[arg(long, conflicts_with = "publish")]
    pub check: bool,
    /// Upload and announce the signed packages after preparation.
    #[arg(long)]
    pub publish: bool,
}

#[derive(Deserialize)]
struct Config {
    catalog: String,
    publisher: String,
}
#[derive(Deserialize)]
struct Prepared {
    spec: PackageSpec,
    payload: PathBuf,
}

pub async fn prepare(args: &ReleaseArgs, home: &Path) -> Result<Option<(PathBuf, String, Keys)>> {
    let config: Config = serde_json::from_slice(&fs::read(&args.config)?)?;
    safe_name(&config.catalog)?;
    let keys = load_keys(
        &args
            .key_file
            .clone()
            .unwrap_or_else(|| home.join("identity.key")),
    )?;
    ensure!(
        keys.public_key() == nostr::PublicKey::parse(&config.publisher)?,
        "release signer does not match configured publisher"
    );
    let stage = tempfile::tempdir()?;
    let python = std::env::var_os("HAPS_PYTHON").unwrap_or_else(|| "python3".into());
    let status = Command::new(python)
        .arg("-c")
        .arg(include_str!("integrations/release.py"))
        .arg(&args.directory)
        .arg(&args.config)
        .arg(stage.path())
        .arg(&args.tag)
        .arg(if args.check { "check" } else { "final" })
        .status()
        .context("release preparation needs Python 3.9 or newer")?;
    ensure!(status.success(), "release preparation failed");
    let plan: Vec<Prepared> = read_json(&stage.path().join("plan.json"))?;
    let directory = home.join("catalogs").join(&config.catalog);
    let mut coordinates = std::collections::BTreeSet::new();
    for package in &plan {
        package.spec.validate()?;
        ensure!(
            coordinates.insert((
                &package.spec.name,
                &package.spec.version,
                &package.spec.target
            )),
            "duplicate package target in release mapping"
        );
    }
    if args.check {
        let temporary = Repository::local(stage.path().join("verified"))?;
        let existing = if directory.join("packages/catalog.json").exists() {
            let repo = Repository::local(directory.join("packages"))?;
            repo.releases(&repo.catalog(&keys.public_key().to_hex()).await?)
                .await?
        } else {
            Vec::new()
        };
        for package in &plan {
            let manifest = temporary
                .pack_payload(&package.spec, &package.payload)
                .await?;
            if let Some(previous) = existing.iter().find(|r| {
                r.data.package.name == package.spec.name
                    && r.data.package.version == package.spec.version
                    && r.data.package.target == package.spec.target
            }) {
                ensure!(
                    previous.data.package == package.spec && previous.data.manifest == manifest,
                    "immutable release differs from its existing package; publish a new version"
                );
            }
        }
        println!("Verified {} package payloads for {}", plan.len(), args.tag);
        return Ok(None);
    }
    let repo = Repository::local(directory.join("packages"))?;
    let discovery = crate::discovery::Discovery::open(&directory)?;
    for package in plan {
        let release = repo
            .publish_reusable(&keys, package.spec, &package.payload)
            .await?;
        println!(
            "Prepared {} {} {}",
            release.data.package.name, release.data.package.version, release.data.package.target
        );
        discovery.ingest([release.event]).await?;
    }
    Ok(Some((directory.join("packages"), config.catalog, keys)))
}
