use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use haps::comments::{self, Comments, Scope};
use haps::{
    install::{Installation, ensure_public_key, load_keys},
    model::*,
    repository::{Repository, Snapshot},
    trust::{Trust, attest},
};
use nostr::{Event, EventBuilder, EventId, Keys, Kind, Tag, Timestamp};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

#[derive(Parser)]
#[command(version, about = "Haps — Hashtree Package System")]
struct Cli {
    #[arg(long, global = true, env = "HAPS_HOME")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create or use a Nostr publishing identity. No account registration.
    Identity {
        #[command(subcommand)]
        action: IdentityAction,
    },
    /// Show this machine's package target.
    Target,
    /// Sign a staged directory and build a shareable hashtree catalog locally.
    Pack {
        manifest: PathBuf,
        #[arg(long)]
        payload: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        key_file: Option<PathBuf>,
    },
    /// Configure catalogs and pin their publishers' public keys.
    Source {
        #[command(subcommand)]
        action: SourceAction,
    },
    /// Search signed hashtree indexes, ranked by your social graph.
    Search {
        query: String,
    },
    /// Inspect the publisher, exact release ID, and social evidence.
    Info {
        package: String,
        #[arg(long)]
        version: Option<semver::Version>,
    },
    /// Install a package. Use publisher/name when a name is ambiguous.
    Install {
        package: String,
        #[arg(long)]
        version: Option<semver::Version>,
        #[arg(long)]
        allow_untrusted: bool,
        #[arg(long, default_value_t = 0)]
        require_attestations: usize,
    },
    /// Update an installed package while retaining its publisher and review policy.
    Update {
        package: String,
        #[arg(long)]
        allow_untrusted: bool,
    },
    /// Run an installed command without changing your system PATH.
    Run {
        package: String,
        #[arg(long)]
        command: Option<String>,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Show an installed directory (including native app bundles).
    Path {
        package: String,
    },
    List,
    /// Activate the previous installed version.
    Rollback {
        package: String,
    },
    /// Remove from the installed set. Retain cached files for recovery.
    Remove {
        package: String,
    },
    /// Follow a publisher locally; optionally export the signed Nostr follow event.
    Follow {
        public_key: String,
        #[arg(long)]
        export: Option<PathBuf>,
    },
    /// Import verified Nostr follows, mutes, and release attestations.
    Import {
        events: PathBuf,
    },
    /// Sign a review of an exact release; no relay publishing is performed.
    Attest {
        release_id: String,
        #[arg(long)]
        note: String,
        #[arg(long)]
        revoke: bool,
        #[arg(long)]
        out: PathBuf,
    },
    /// Write a signed package comment, release comment, or reply.
    Comment {
        package: String,
        text: String,
        /// Target an exact release event ID instead of the whole package.
        #[arg(long)]
        release: Option<String>,
        #[arg(long)]
        reply_to: Option<String>,
        /// Export the NIP-22 event for sharing. No relay publishing.
        #[arg(long)]
        out: PathBuf,
    },
    /// Read imported comments, with social context and local mutes applied.
    Comments {
        package: String,
        #[arg(long)]
        release: Option<String>,
    },
}

#[derive(Subcommand)]
enum IdentityAction {
    Init,
    /// Use a public key for discovery without storing its secret key.
    Use {
        public_key: String,
    },
    Show,
}

#[derive(Subcommand)]
enum SourceAction {
    Add {
        name: String,
        location: String,
        #[arg(long)]
        author: String,
    },
    List,
    Remove {
        name: String,
    },
}

#[derive(Default, Serialize, Deserialize)]
struct Config {
    identity: Option<String>,
    sources: BTreeMap<String, Source>,
}

#[derive(Serialize, Deserialize)]
struct Source {
    location: String,
    author: String,
    sequence: u64,
    event_id: String,
}

struct Candidate {
    source: String,
    release: Release,
}

fn save_config(home: &Path, config: &Config) -> Result<()> {
    atomic_write(
        &home.join("config.json"),
        &serde_json::to_vec_pretty(config)?,
    )
}

fn load_trust(home: &Path, config: &Config) -> Result<Trust> {
    // An unset identity has no author relationships; explicit installs can still
    // proceed with --allow-untrusted after inspection.
    let mut trust = Trust::new(
        config
            .identity
            .clone()
            .unwrap_or_else(|| "unconfigured".into()),
    );
    let file = home.join("social.json");
    if file.exists() {
        let events: Vec<Event> = read_json(&file)?;
        ensure!(events.len() <= 10_000, "social event limit exceeded");
        for event in events {
            trust.ingest(event)?;
        }
    }
    Ok(trust)
}

fn save_trust(home: &Path, trust: &Trust) -> Result<()> {
    atomic_write(
        &home.join("social.json"),
        &serde_json::to_vec_pretty(&trust.events())?,
    )
}

fn load_comments(home: &Path) -> Result<Comments> {
    let mut comments = Comments::default();
    let path = home.join("comments.json");
    if path.exists() {
        for event in read_json::<Vec<Event>>(&path)? {
            comments.ingest(event)?;
        }
    }
    Ok(comments)
}

fn save_comments(home: &Path, comments: &Comments) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(&comments.events())?;
    ensure!(
        bytes.len() <= MAX_METADATA,
        "local comments exceed metadata limit"
    );
    atomic_write(&home.join("comments.json"), &bytes)
}

fn select_discussion(
    candidates: Vec<Candidate>,
    package: &str,
    release: Option<&str>,
) -> Result<Candidate> {
    if let Some(id) = release {
        let id = EventId::from_hex(id)?;
        let filtered: Vec<_> = candidates
            .into_iter()
            .filter(|c| c.release.event.id == id)
            .collect();
        let normalized = if let Some((key, name)) = package.split_once('/') {
            format!("{}/{}", ensure_public_key(key)?, name)
        } else {
            package.into()
        };
        let candidate = filtered
            .into_iter()
            .find(|c| {
                c.release.identity() == normalized || c.release.data.package.name == normalized
            })
            .context("release does not belong to this package or is unavailable")?;
        return Ok(candidate);
    }
    // Package discussion is shared across operating systems and versions.
    let normalized = if let Some((key, name)) = package.split_once('/') {
        format!("{}/{}", ensure_public_key(key)?, name)
    } else {
        package.into()
    };
    let mut filtered: Vec<_> = candidates
        .into_iter()
        .filter(|c| c.release.identity() == normalized || c.release.data.package.name == normalized)
        .collect();
    let authors: BTreeSet<_> = filtered.iter().map(|c| c.release.author()).collect();
    ensure!(
        authors.len() == 1,
        "package missing or ambiguous; use publisher/name"
    );
    filtered.sort_by(|a, b| {
        b.release
            .data
            .package
            .version
            .cmp(&a.release.data.package.version)
    });
    Ok(filtered.remove(0))
}

fn own_keys(home: &Path, config: &Config) -> Result<Keys> {
    let keys = load_keys(&home.join("identity.key"))?;
    ensure!(
        config.identity.as_deref() == Some(keys.public_key().to_hex().as_str()),
        "active identity does not match local signing key"
    );
    Ok(keys)
}

fn check_checkpoint(source: &mut Source, snapshot: &Snapshot) -> Result<()> {
    ensure!(
        snapshot.head.sequence >= source.sequence,
        "catalog rollback detected"
    );
    ensure!(
        snapshot.head.sequence != source.sequence || snapshot.event.id.to_hex() == source.event_id,
        "catalog changed at the same sequence"
    );
    source.sequence = snapshot.head.sequence;
    source.event_id = snapshot.event.id.to_hex();
    Ok(())
}

async fn candidates(
    home: &Path,
    config: &mut Config,
    query: Option<&str>,
) -> Result<Vec<Candidate>> {
    ensure!(
        !config.sources.is_empty(),
        "no sources configured; use `haps source add`"
    );
    let mut found = Vec::new();
    let mut seen = BTreeMap::new();
    for (name, source) in &mut config.sources {
        let repo = Repository::open(&source.location, &home.join("cache"))?;
        let snapshot = repo
            .catalog(&source.author)
            .await
            .with_context(|| format!("source {name} is unavailable or invalid"))?;
        check_checkpoint(source, &snapshot)?;
        let releases = if let Some(query) = query {
            repo.search(&snapshot, query).await?
        } else {
            repo.releases(&snapshot).await?
        };
        for release in releases {
            if let Some(id) = seen.insert(release.coordinate(), release.event.id) {
                ensure!(
                    id == release.event.id,
                    "conflicting signed releases at {}",
                    release.coordinate()
                );
                continue;
            }
            found.push(Candidate {
                source: name.clone(),
                release,
            });
        }
    }
    save_config(home, config)?;
    Ok(found)
}

fn select(
    mut candidates: Vec<Candidate>,
    package: &str,
    version: Option<&semver::Version>,
) -> Result<Candidate> {
    let package = if let Some((key, name)) = package.split_once('/') {
        format!("{}/{}", ensure_public_key(key)?, name)
    } else {
        package.to_string()
    };
    candidates.retain(|c| {
        (c.release.identity() == package || c.release.data.package.name == package)
            && c.release.data.package.target == target()
            && version.is_none_or(|v| &c.release.data.package.version == v)
            && (version.is_some() || c.release.data.package.version.pre.is_empty())
    });
    let authors: BTreeSet<_> = candidates.iter().map(|c| c.release.author()).collect();
    ensure!(
        !candidates.is_empty(),
        "no matching release for {}",
        target()
    );
    ensure!(
        authors.len() == 1,
        "multiple publishers use this name; select publisher/name after inspecting `haps search`"
    );
    candidates.sort_by(|a, b| {
        b.release
            .data
            .package
            .version
            .cmp(&a.release.data.package.version)
    });
    Ok(candidates.remove(0))
}

fn print_release(release: &Release, trust: &Trust) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "package": release.data.package, "publisher": release.author(),
            "release_id": release.event.id.to_hex(), "manifest": release.data.manifest,
            "follow_distance": trust.distance(&release.author()), "muted": trust.muted(&release.author()),
            "attesters": trust.attesters(release),
        }))?
    );
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    match execute(Cli::parse()).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("haps: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn execute(cli: Cli) -> Result<u8> {
    if matches!(cli.command, Command::Target) {
        println!("{}", target());
        return Ok(0);
    }
    let home = cli
        .home
        .or_else(|| dirs::data_local_dir().map(|p| p.join("haps")))
        .context("set HAPS_HOME or --home")?;
    fs::create_dir_all(&home)?;
    let home = home.canonicalize()?;
    let guard = lock(&home.join(".cli.lock"))?;
    let config_file = home.join("config.json");
    let mut config: Config = if config_file.exists() {
        read_json(&config_file)?
    } else {
        Config::default()
    };
    let mut trust = load_trust(&home, &config)?;
    let installation = Installation::new(home.clone())?;
    match cli.command {
        Command::Target => unreachable!(),
        Command::Identity { action } => match action {
            IdentityAction::Init => {
                let path = home.join("identity.key");
                ensure!(
                    !path.exists(),
                    "identity already exists; use `haps identity show`"
                );
                let keys = Keys::generate();
                use std::io::Write;
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(path)?;
                file.write_all(keys.secret_key().to_secret_hex().as_bytes())?;
                file.sync_all()?;
                config.identity = Some(keys.public_key().to_hex());
                save_config(&home, &config)?;
                println!("{}", keys.public_key().to_hex());
            }
            IdentityAction::Use { public_key } => {
                config.identity = Some(ensure_public_key(&public_key)?);
                save_config(&home, &config)?;
                println!("{}", config.identity.unwrap());
            }
            IdentityAction::Show => {
                println!("{}", config.identity.context("no identity configured")?)
            }
        },
        Command::Pack {
            manifest,
            payload,
            out,
            key_file,
        } => {
            let spec: PackageSpec = toml::from_str(&fs::read_to_string(manifest)?)?;
            let keys = load_keys(&key_file.unwrap_or_else(|| home.join("identity.key")))?;
            let repo = Repository::local(out)?;
            let release = repo.publish(&keys, spec, &payload).await?;
            println!("{}", serde_json::to_string_pretty(&release.event)?);
        }
        Command::Source { action } => match action {
            SourceAction::Add {
                name,
                location,
                author,
            } => {
                safe_name(&name)?;
                ensure!(
                    !config.sources.contains_key(&name),
                    "source already exists; remove it explicitly before replacing its publisher pin"
                );
                let author = ensure_public_key(&author)?;
                let location =
                    if location.starts_with("https://") || location.starts_with("http://") {
                        location
                    } else {
                        Path::new(&location)
                            .canonicalize()?
                            .to_str()
                            .context("non-UTF8 source path")?
                            .to_string()
                    };
                let repo = Repository::open(&location, &home.join("cache"))?;
                let snapshot = repo.catalog(&author).await?;
                config.sources.insert(
                    name.clone(),
                    Source {
                        location,
                        author,
                        sequence: snapshot.head.sequence,
                        event_id: snapshot.event.id.to_hex(),
                    },
                );
                save_config(&home, &config)?;
                println!("Added {name}");
            }
            SourceAction::List => println!("{}", serde_json::to_string_pretty(&config.sources)?),
            SourceAction::Remove { name } => {
                ensure!(config.sources.remove(&name).is_some(), "source not found");
                save_config(&home, &config)?;
            }
        },
        Command::Search { query } => {
            let mut results = candidates(&home, &mut config, Some(&query)).await?;
            results.retain(|c| !trust.muted(&c.release.author()));
            // Unknown authors sort after known authors; fewer hops first.
            results.sort_by_key(|c| {
                (
                    trust.distance(&c.release.author()).unwrap_or(u32::MAX),
                    std::cmp::Reverse(trust.attesters(&c.release).len()),
                )
            });
            for candidate in results {
                print_release(&candidate.release, &trust)?;
            }
        }
        Command::Info { package, version } => {
            let candidate = select(
                candidates(&home, &mut config, None).await?,
                &package,
                version.as_ref(),
            )?;
            print_release(&candidate.release, &trust)?;
        }
        Command::Install {
            package,
            version,
            allow_untrusted,
            require_attestations,
        } => {
            let candidate = select(
                candidates(&home, &mut config, None).await?,
                &package,
                version.as_ref(),
            )?;
            let require_attestations = require_attestations.max(
                installation
                    .receipts()?
                    .get(&candidate.release.identity())
                    .map_or(0, |r| r.minimum_attestations),
            );
            trust.authorize(&candidate.release, allow_untrusted, require_attestations)?;
            let repo = Repository::open(
                &config.sources[&candidate.source].location,
                &home.join("cache"),
            )?;
            installation
                .install_with_policy(&repo, &candidate.release, require_attestations)
                .await?;
            println!(
                "Installed {} {}",
                candidate.release.identity(),
                candidate.release.data.package.version
            );
        }
        Command::Update {
            package,
            allow_untrusted,
        } => {
            let receipt = installation.receipt(&package)?;
            let current = Release::verify(receipt.current)?;
            let candidate = select(
                candidates(&home, &mut config, None).await?,
                &current.identity(),
                None,
            )?;
            trust.authorize(
                &candidate.release,
                allow_untrusted,
                receipt.minimum_attestations,
            )?;
            installation
                .install_with_policy(
                    &Repository::open(
                        &config.sources[&candidate.source].location,
                        &home.join("cache"),
                    )?,
                    &candidate.release,
                    receipt.minimum_attestations,
                )
                .await?;
            println!(
                "Installed {} {}",
                candidate.release.identity(),
                candidate.release.data.package.version
            );
        }
        Command::Run {
            package,
            command,
            args,
        } => {
            let executable = installation.command(&package, command.as_deref())?;
            drop(guard);
            let status = std::process::Command::new(executable).args(args).status()?;
            return Ok(status
                .code()
                .and_then(|c| u8::try_from(c).ok())
                .unwrap_or(1));
        }
        Command::Path { package } => println!("{}", installation.path(&package)?.display()),
        Command::List => {
            for (id, receipt) in installation.receipts()? {
                let release = Release::verify(receipt.current)?;
                println!(
                    "{id}\t{}\t{}",
                    release.data.package.version, release.event.id
                );
            }
        }
        Command::Rollback { package } => {
            installation.rollback(&package)?;
            println!("Rolled back {package}");
        }
        Command::Remove { package } => {
            installation.remove(&package)?;
            println!("Removed {package}");
        }
        Command::Follow { public_key, export } => {
            let keys = own_keys(&home, &config)?;
            let public_key = nostr::PublicKey::parse(&public_key)?;
            let old = trust
                .events()
                .into_iter()
                .find(|e| e.kind == Kind::ContactList && e.pubkey == keys.public_key());
            let mut tags: Vec<Tag> = old
                .as_ref()
                .map(|e| e.tags.iter().cloned().collect())
                .unwrap_or_default();
            if !tags.iter().any(|t| {
                t.as_slice().first().is_some_and(|v| v == "p")
                    && t.as_slice().get(1) == Some(&public_key.to_hex())
            }) {
                tags.push(Tag::public_key(public_key));
            }
            let timestamp = Timestamp::from(
                Timestamp::now().as_secs().max(
                    old.as_ref()
                        .map(|e| e.created_at.as_secs() + 1)
                        .unwrap_or(0),
                ),
            );
            let event = EventBuilder::new(
                Kind::ContactList,
                old.as_ref().map(|e| e.content.as_str()).unwrap_or(""),
            )
            .tags(tags)
            .custom_created_at(timestamp)
            .sign_with_keys(&keys)?;
            trust.ingest(event.clone())?;
            save_trust(&home, &trust)?;
            if let Some(path) = export {
                atomic_write(&path, &serde_json::to_vec_pretty(&event)?)?;
            }
            println!("Following {} locally", public_key.to_hex());
        }
        Command::Import { events } => {
            let value: serde_json::Value = read_json(&events)?;
            let events: Vec<Event> = if value.is_array() {
                serde_json::from_value(value)?
            } else {
                vec![serde_json::from_value(value)?]
            };
            ensure!(events.len() <= 10_000, "too many events");
            let count = events.len();
            let mut comments = load_comments(&home)?;
            for event in events {
                if event.kind == Kind::Comment {
                    comments.ingest(event)?;
                } else {
                    trust.ingest(event)?;
                }
            }
            save_trust(&home, &trust)?;
            save_comments(&home, &comments)?;
            println!("Imported {count} signed events");
        }
        Command::Attest {
            release_id,
            note,
            revoke,
            out,
        } => {
            let keys = own_keys(&home, &config)?;
            let mut event = attest(&keys, EventId::from_hex(&release_id)?, !revoke, note)?;
            let identifier = tag_value(&event, "d")?;
            let previous = trust
                .events()
                .into_iter()
                .filter(|e| {
                    e.pubkey == keys.public_key()
                        && e.kind == APP_KIND
                        && tag_value(e, "d").ok() == Some(identifier)
                })
                .map(|e| e.created_at.as_secs())
                .max();
            if let Some(previous) = previous
                && previous >= event.created_at.as_secs()
            {
                event = EventBuilder::new(APP_KIND, event.content.clone())
                    .tags(event.tags.clone())
                    .custom_created_at(Timestamp::from(previous + 1))
                    .sign_with_keys(&keys)?;
            }
            trust.ingest(event.clone())?;
            save_trust(&home, &trust)?;
            atomic_write(&out, &serde_json::to_vec_pretty(&event)?)?;
            println!("{}", event.id);
        }
        Command::Comment {
            package,
            text,
            release,
            reply_to,
            out,
        } => {
            let candidate = select_discussion(
                candidates(&home, &mut config, None).await?,
                &package,
                release.as_deref(),
            )?;
            let mut comments = load_comments(&home)?;
            let parent = reply_to.as_deref().map(EventId::from_hex).transpose()?;
            let parent = parent
                .map(|id| comments.get(&id).context("reply target is not imported"))
                .transpose()?;
            let root = if release.is_some() {
                candidate.release.event.clone()
            } else {
                let source = &config.sources[&candidate.source];
                let repo = Repository::open(&source.location, &home.join("cache"))?;
                let snapshot = repo.catalog(&source.author).await?;
                let cid = snapshot
                    .catalog
                    .packages
                    .get(&candidate.release.identity())
                    .context("package card is missing")?;
                let event: Event = repo.json(cid).await?;
                ensure!(
                    event.pubkey == candidate.release.event.pubkey
                        && tag_value(&event, "d")?
                            == format!("haps/package/{}", candidate.release.data.package.name),
                    "package card publisher/name mismatch"
                );
                event
            };
            let event = comments::create(
                &own_keys(&home, &config)?,
                &root,
                release.is_some(),
                parent,
                text,
            )?;
            comments.ingest(event.clone())?;
            save_comments(&home, &comments)?;
            atomic_write(&out, &serde_json::to_vec_pretty(&event)?)?;
            println!("{}", event.id);
        }
        Command::Comments { package, release } => {
            let candidate = select_discussion(
                candidates(&home, &mut config, None).await?,
                &package,
                release.as_deref(),
            )?;
            let scope = if release.is_some() {
                Scope::release(&candidate.release)
            } else {
                Scope::package(&candidate.release)
            };
            let comments = load_comments(&home)?;
            for event in comments.thread(&scope) {
                if trust.muted(&event.pubkey.to_hex()) {
                    continue;
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "event": event, "follow_distance": trust.distance(&event.pubkey.to_hex()),
                    }))?
                );
            }
        }
    }
    Ok(0)
}
