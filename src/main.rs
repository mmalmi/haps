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
    /// Start a fresh configuration without the maintainer catalog or trust seed.
    #[arg(long, global = true, env = "HAPS_NO_DEFAULTS")]
    no_defaults: bool,
    /// Never prompt; require publisher/name when several publishers match.
    #[arg(long, global = true, env = "HAPS_NON_INTERACTIVE")]
    non_interactive: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show, replace, or disable the local social-graph starting point.
    StartingPoint {
        public_key: Option<String>,
        #[arg(long, conflicts_with = "public_key")]
        clear: bool,
    },
    /// Create or use a Nostr publishing identity. No account registration.
    Identity {
        #[command(subcommand)]
        action: IdentityAction,
    },
    /// Give a publisher a local name, such as alice/package.
    Alias {
        #[command(subcommand)]
        action: AliasAction,
    },
    /// Show this machine's package target.
    Target,
    /// Create a package manifest for a staged binary or macOS app.
    Init(haps::init::InitArgs),
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
    /// Inspect or explicitly execute a build recipe from a pinned Git checkout.
    Build {
        repository: String,
        #[arg(long)]
        rev: String,
        #[arg(long, default_value = "haps-build.toml")]
        recipe: String,
        /// Run the recipe with your user permissions. Builds are not sandboxed.
        #[arg(long)]
        execute: bool,
        /// Install the locally signed build after it succeeds.
        #[arg(long, requires = "execute")]
        install: bool,
    },
    /// Configure catalogs and pin their publishers' public keys.
    Source {
        #[command(subcommand)]
        action: SourceAction,
    },
    /// Search signed hashtree indexes, ranked by your social graph.
    Search {
        query: String,
        /// Emit one JSON array and never prompt.
        #[arg(long)]
        json: bool,
    },
    /// Inspect the publisher, exact release ID, and social evidence.
    Info {
        package: String,
        #[arg(long)]
        version: Option<semver::Version>,
        /// Emit JSON and never prompt.
        #[arg(long)]
        json: bool,
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
        /// Emit JSON and never prompt.
        #[arg(long)]
        json: bool,
    },
    /// Update an installed package while retaining its publisher and attestation policy.
    Update {
        package: String,
        #[arg(long)]
        allow_untrusted: bool,
        /// Emit JSON and never prompt.
        #[arg(long)]
        json: bool,
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
    Path { package: String },
    /// Add a small Haps submenu to Omarchy v4 (requires Python 3 and fzf).
    Omarchy {
        /// Remove only the menu entries installed by Haps.
        #[arg(long)]
        remove: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    /// Activate the previous installed version.
    Rollback { package: String },
    /// Remove from the installed set. Retain cached files for recovery.
    Remove { package: String },
    /// Follow a publisher locally; optionally export the signed Nostr follow event.
    Follow {
        public_key: String,
        #[arg(long)]
        export: Option<PathBuf>,
    },
    /// Import verified Nostr follows, mutes, and release attestations.
    Import { events: PathBuf },
    /// Sign a claim about an exact release; no relay publishing is performed.
    Attest {
        /// Release event ID, or publisher/package with --version.
        release: String,
        /// Required with a package name. Uses this machine's platform by default.
        #[arg(long)]
        version: Option<semver::Version>,
        /// Select another platform when using a package name and --version.
        #[arg(long, requires = "version")]
        target: Option<String>,
        #[arg(long)]
        note: String,
        #[arg(long)]
        revoke: bool,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Emit the signed event as JSON and never prompt.
        #[arg(long)]
        json: bool,
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

impl Command {
    fn json(&self) -> bool {
        match self {
            Self::Init(args) => args.json,
            Self::Search { json, .. }
            | Self::Info { json, .. }
            | Self::Install { json, .. }
            | Self::Update { json, .. }
            | Self::List { json }
            | Self::Attest { json, .. } => *json,
            _ => false,
        }
    }
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

#[derive(Subcommand)]
enum AliasAction {
    Add { name: String, public_key: String },
    List,
    Remove { name: String },
}

#[derive(Default, Serialize, Deserialize)]
struct Config {
    identity: Option<String>,
    sources: BTreeMap<String, Source>,
    #[serde(skip)]
    aliases: BTreeMap<String, String>,
    #[serde(default)]
    starting_point: Option<String>,
}

fn fresh_config(no_defaults: bool) -> Result<Config> {
    let mut config = Config::default();
    if !no_defaults {
        let npub = hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB;
        let key = ensure_public_key(npub)?;
        config.starting_point = Some(key.clone());
        config.sources.insert(
            "iris".into(),
            Source {
                location: format!("htree://{npub}/haps-packages"),
                author: key,
                sequence: 0,
                event_id: String::new(),
            },
        );
    }
    Ok(config)
}

fn resolve_key(config: &Config, key: &str) -> Result<String> {
    ensure_public_key(config.aliases.get(key).map(String::as_str).unwrap_or(key))
}

fn resolve_package(config: &Config, package: &str) -> Result<String> {
    if let Some((key, name)) = package.split_once('/') {
        safe_name(name)?;
        return Ok(format!("{}/{}", resolve_key(config, key)?, name));
    }
    Ok(package.into())
}

fn publisher_label(aliases: &BTreeMap<String, String>, author: &str) -> String {
    use nostr::nips::nip19::ToBech32;
    aliases
        .iter()
        .find(|(_, key)| key.as_str() == author)
        .map(|(alias, _)| alias.clone())
        .unwrap_or_else(|| {
            nostr::PublicKey::parse(author)
                .map(|p| p.to_bech32().unwrap())
                .unwrap_or_else(|_| author.into())
        })
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
    trust.set_starting_point(config.starting_point.clone())?;
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
        source.event_id.is_empty()
            || snapshot.head.sequence != source.sequence
            || snapshot.event.id.to_hex() == source.event_id,
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
    aliases: &BTreeMap<String, String>,
    trust: &Trust,
    target_filter: &str,
    non_interactive: bool,
) -> Result<Candidate> {
    let package = if let Some((key, name)) = package.split_once('/') {
        format!("{}/{}", ensure_public_key(key)?, name)
    } else {
        package.to_string()
    };
    candidates.retain(|c| {
        (c.release.identity() == package || c.release.data.package.name == package)
            && c.release.data.package.target == target_filter
            && version.is_none_or(|v| &c.release.data.package.version == v)
            && (version.is_some() || c.release.data.package.version.pre.is_empty())
    });
    let authors: BTreeSet<_> = candidates.iter().map(|c| c.release.author()).collect();
    ensure!(
        !candidates.is_empty(),
        "no matching release for {}",
        target_filter
    );
    if authors.len() > 1 {
        use std::io::IsTerminal;
        let mut choices: Vec<_> = authors.into_iter().collect();
        let approvals = |author: &str| {
            candidates
                .iter()
                .filter(|c| c.release.author() == author)
                .max_by(|a, b| {
                    a.release
                        .data
                        .package
                        .version
                        .cmp(&b.release.data.package.version)
                })
                .map_or(0, |c| trust.attesters(&c.release).len())
        };
        choices.sort_by_key(|author| {
            (
                trust.muted(author),
                trust.distance(author).unwrap_or(u32::MAX),
                std::cmp::Reverse(approvals(author)),
                publisher_label(aliases, author),
            )
        });
        let releases: Vec<_> = choices
            .iter()
            .map(|author| {
                &candidates
                    .iter()
                    .filter(|c| c.release.author() == *author)
                    .max_by_key(|c| &c.release.data.package.version)
                    .unwrap()
                    .release
            })
            .collect();
        let options: Vec<_> = releases
            .iter()
            .map(|release| {
                let author = release.author();
                format!(
                    "{} {} · {}{}\n    {}",
                    release_label(release, aliases),
                    release.data.package.version,
                    relationship(trust, &author, aliases),
                    if trust.muted(&author) {
                        " · muted"
                    } else {
                        ""
                    },
                    attestation_summary(release, trust, aliases),
                )
            })
            .collect();
        if non_interactive
            || !std::io::stdin().is_terminal()
            || !std::io::stdout().is_terminal()
            || !std::io::stderr().is_terminal()
            || std::env::var("TERM").is_ok_and(|term| term == "dumb")
        {
            return Err(AmbiguousPublishers {
                candidates: releases
                    .iter()
                    .map(|r| release_json(r, trust, aliases))
                    .collect(),
            }
            .into());
        }
        eprintln!("Several publishers match. Choose one:");
        eprintln!("↑/↓ move · Enter select · Esc cancel");
        let theme = dialoguer::theme::ColorfulTheme {
            active_item_style: dialoguer::console::Style::new().for_stderr().green(),
            ..Default::default()
        };
        let index = dialoguer::Select::with_theme(&theme)
            .items(&options)
            .default(0)
            .max_length(5)
            .interact_opt()?
            .context("cancelled; no publisher selected")?;
        candidates.retain(|c| c.release.author() == choices[index]);
    }
    candidates.sort_by(|a, b| {
        b.release
            .data
            .package
            .version
            .cmp(&a.release.data.package.version)
    });
    Ok(candidates.remove(0))
}

// Terminal text must not execute control sequences from shared aliases or signed notes.
fn terminal_text(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| {
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn release_label(release: &Release, aliases: &BTreeMap<String, String>) -> String {
    format!(
        "{}/{}",
        terminal_text(&publisher_label(aliases, &release.author())),
        release.data.package.name
    )
}

fn relationship(trust: &Trust, author: &str, aliases: &BTreeMap<String, String>) -> String {
    if trust.distance(author) == Some(0) {
        return "You".into();
    }
    let mut names: Vec<_> = trust
        .followed_by_friends(author)
        .iter()
        .map(|key| terminal_text(&publisher_label(aliases, key)))
        .collect();
    names.sort();
    if !names.is_empty() {
        let remaining = names.len().saturating_sub(3);
        names.truncate(3);
        let suffix = match remaining {
            0 => String::new(),
            1 => " and 1 other you follow".into(),
            n => format!(" and {n} others you follow"),
        };
        return format!("Followed by {}{suffix}", names.join(", "));
    }
    match trust.distance(author) {
        Some(1) => "Followed by you".into(),
        Some(_) => "In your extended graph".into(),
        None => "outside your graph".into(),
    }
}

fn attestation_summary(
    release: &Release,
    trust: &Trust,
    aliases: &BTreeMap<String, String>,
) -> String {
    let mut names: Vec<_> = trust
        .attesters(release)
        .iter()
        .map(|key| terminal_text(&publisher_label(aliases, key)))
        .collect();
    names.sort();
    if names.is_empty() {
        "No trusted attestations".into()
    } else {
        format!("Attested by {}", names.join(", "))
    }
}

fn release_json(
    release: &Release,
    trust: &Trust,
    aliases: &BTreeMap<String, String>,
) -> serde_json::Value {
    let attestations: Vec<_> = trust
        .attestations(release)
        .iter()
        .map(|event| {
            serde_json::json!({"signer": event.pubkey.to_hex(),
            "label": publisher_label(aliases, &event.pubkey.to_hex()), "event": event})
        })
        .collect();
    serde_json::json!({
        "identity": release.identity(),
        "label": release_label(release, aliases),
        "package": release.data.package, "publisher": release.author(),
        "release_id": release.event.id.to_hex(), "manifest": release.data.manifest,
        "follow_distance": trust.distance(&release.author()), "muted": trust.muted(&release.author()),
        "followed_by": trust.followed_by_friends(&release.author()).iter().map(|key| serde_json::json!({"pubkey": key, "label": publisher_label(aliases, key)})).collect::<Vec<_>>(),
        "attesters": trust.attesters(release), "attestations": attestations,
    })
}

#[derive(Debug)]
struct AmbiguousPublishers {
    candidates: Vec<serde_json::Value>,
}

impl std::fmt::Display for AmbiguousPublishers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "multiple publishers use this name; choose an explicit publisher/name:"
        )?;
        for candidate in &self.candidates {
            writeln!(
                f,
                "  {} ({})",
                candidate["label"].as_str().unwrap(),
                candidate["identity"].as_str().unwrap()
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for AmbiguousPublishers {}

fn print_release(
    release: &Release,
    trust: &Trust,
    aliases: &BTreeMap<String, String>,
) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&release_json(release, trust, aliases))?
    );
    Ok(())
}

fn print_install_start(release: &Release, trust: &Trust, aliases: &BTreeMap<String, String>) {
    eprintln!(
        "Installing {} {}",
        release_label(release, aliases),
        release.data.package.version
    );
    eprintln!("  Signature verified");
    eprintln!("  {}", attestation_summary(release, trust, aliases));
    for event in trust.attestations(release) {
        if let Ok(claim) = serde_json::from_str::<haps::trust::Attestation>(&event.content) {
            eprintln!(
                "    {}: {}",
                terminal_text(&publisher_label(aliases, &event.pubkey.to_hex())),
                terminal_text(&claim.note)
            );
        }
    }
}

fn print_installed(
    release: &Release,
    trust: &Trust,
    aliases: &BTreeMap<String, String>,
    json: bool,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::json!({"status": "installed", "release": release_json(release, trust, aliases)})
        );
    } else {
        println!(
            "Installed {} {}",
            release_label(release, aliases),
            release.data.package.version
        );
        if release.data.package.app.is_some() || !release.data.package.commands.is_empty() {
            println!("Run with: haps run {}", release_label(release, aliases));
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.command.json();
    match execute(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            if json {
                let mut value = serde_json::json!({"code": "operation_failed", "message": format!("{error:#}")});
                if let Some(ambiguous) = error.downcast_ref::<AmbiguousPublishers>() {
                    value["code"] = "ambiguous_package".into();
                    value["candidates"] = serde_json::json!(ambiguous.candidates);
                }
                println!("{}", serde_json::json!({"error": value}));
            } else {
                eprintln!("haps: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

async fn execute(mut cli: Cli) -> Result<u8> {
    let non_interactive = cli.non_interactive || cli.command.json();
    if let Command::Init(args) = &cli.command {
        let spec = haps::init::create(args)?;
        if args.json {
            println!(
                "{}",
                serde_json::to_string(
                    &serde_json::json!({"status": "created", "manifest": args.out, "package": spec})
                )?
            );
        } else {
            println!("Created {}", args.out.display());
            println!(
                "Check the metadata, then pack it with `haps pack <manifest> --payload <directory> --out <catalog>`."
            );
        }
        return Ok(0);
    }
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
        fresh_config(cli.no_defaults)?
    };
    config.aliases = haps::aliases::read()?;
    if !config_file.exists() {
        if config.starting_point.is_some() {
            let name = hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_ALIAS;
            if !config.aliases.contains_key(name) {
                haps::aliases::add(name, hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB)?;
                config.aliases = haps::aliases::read()?;
            }
            eprintln!(
                "Starting point: Sirius Business Ltd (maintainer). Use `haps starting-point` to inspect or change it; `haps starting-point --clear` disables it."
            );
        }
        save_config(&home, &config)?;
    }
    let mut trust = load_trust(&home, &config)?;
    match &mut cli.command {
        Command::Install { package, .. }
        | Command::Info { package, .. }
        | Command::Update { package, .. }
        | Command::Run { package, .. }
        | Command::Path { package }
        | Command::Rollback { package }
        | Command::Remove { package }
        | Command::Comment { package, .. }
        | Command::Comments { package, .. } => {
            *package = resolve_package(&config, package)?;
        }
        _ => {}
    }
    let installation = Installation::new(home.clone())?;
    match cli.command {
        Command::Target | Command::Init(_) => unreachable!(),
        Command::StartingPoint { public_key, clear } => {
            if clear {
                config.starting_point = None;
                save_config(&home, &config)?;
                println!("Starting point disabled");
            } else if let Some(key) = public_key {
                config.starting_point = Some(resolve_key(&config, &key)?);
                save_config(&home, &config)?;
            }
            println!("{}", config.starting_point.as_deref().unwrap_or("none"));
        }
        Command::Alias { action } => match action {
            AliasAction::Add { name, public_key } => {
                let key = ensure_public_key(&public_key)?;
                haps::aliases::add(&name, &key)?;
                println!("{name}: {key}");
            }
            AliasAction::List => println!("{}", serde_json::to_string_pretty(&config.aliases)?),
            AliasAction::Remove { name } => {
                haps::aliases::remove(&name)?;
            }
        },
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
        Command::Build {
            repository,
            rev,
            recipe,
            execute,
            install,
        } => {
            let checkout = haps::build::Checkout::fetch(
                SourceInfo {
                    git: repository,
                    rev,
                },
                &recipe,
                &home.join("builds"),
            )?;
            println!("{}", serde_json::to_string_pretty(&checkout.recipe)?);
            if execute {
                let keys = own_keys(&home, &config)?;
                let payload = checkout.execute()?;
                let repo = Repository::local(home.join("built-packages"))?;
                let release = repo
                    .publish(&keys, checkout.recipe.package.clone(), &payload)
                    .await?;
                if install {
                    installation.install(&repo, &release).await?;
                }
                println!("Built {} {}", release.identity(), release.event.id);
            } else {
                println!(
                    "Preview only. Add --execute to run these commands with your user permissions; add --install to activate the result. Build recipes are not sandboxed."
                );
            }
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
                let author = resolve_key(&config, &author)?;
                let location = if location.starts_with("https://")
                    || location.starts_with("http://")
                    || location.starts_with("htree://")
                {
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
        Command::Search { query, json } => {
            let mut results = candidates(&home, &mut config, Some(&query)).await?;
            results.retain(|c| !trust.muted(&c.release.author()));
            // Unknown authors sort after known authors; fewer hops first.
            results.sort_by_key(|c| {
                (
                    trust.distance(&c.release.author()).unwrap_or(u32::MAX),
                    std::cmp::Reverse(trust.attesters(&c.release).len()),
                )
            });
            if json {
                let releases: Vec<_> = results
                    .iter()
                    .map(|c| release_json(&c.release, &trust, &config.aliases))
                    .collect();
                println!("{}", serde_json::to_string(&releases)?);
            } else {
                for candidate in results {
                    print_release(&candidate.release, &trust, &config.aliases)?;
                }
            }
        }
        Command::Info {
            package, version, ..
        } => {
            let candidate = select(
                candidates(&home, &mut config, None).await?,
                &package,
                version.as_ref(),
                &config.aliases,
                &trust,
                target(),
                non_interactive,
            )?;
            print_release(&candidate.release, &trust, &config.aliases)?;
        }
        Command::Install {
            package,
            version,
            allow_untrusted,
            require_attestations,
            json,
        } => {
            let candidate = select(
                candidates(&home, &mut config, None).await?,
                &package,
                version.as_ref(),
                &config.aliases,
                &trust,
                target(),
                non_interactive,
            )?;
            let require_attestations = require_attestations.max(
                installation
                    .receipts()?
                    .get(&candidate.release.identity())
                    .map_or(0, |r| r.minimum_attestations),
            );
            trust.authorize(&candidate.release, allow_untrusted, require_attestations)?;
            if !json {
                print_install_start(&candidate.release, &trust, &config.aliases);
            }
            let repo = Repository::open(
                &config.sources[&candidate.source].location,
                &home.join("cache"),
            )?;
            installation
                .install_with_policy(&repo, &candidate.release, require_attestations)
                .await?;
            print_installed(&candidate.release, &trust, &config.aliases, json)?;
        }
        Command::Update {
            package,
            allow_untrusted,
            json,
        } => {
            let receipt = installation.receipt(&package)?;
            let current = Release::verify(receipt.current)?;
            let candidate = select(
                candidates(&home, &mut config, None).await?,
                &current.identity(),
                None,
                &config.aliases,
                &trust,
                target(),
                non_interactive,
            )?;
            trust.authorize(
                &candidate.release,
                allow_untrusted,
                receipt.minimum_attestations,
            )?;
            if !json {
                print_install_start(&candidate.release, &trust, &config.aliases);
            }
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
            print_installed(&candidate.release, &trust, &config.aliases, json)?;
        }
        Command::Run {
            package,
            command,
            args,
        } => {
            #[cfg(target_os = "macos")]
            if command.is_none() {
                let release = Release::verify(installation.receipt(&package)?.current)?;
                if let Some(app) = release.data.package.app {
                    let path = installation.path(&package)?.join(safe_path(&app)?);
                    drop(guard);
                    let status = std::process::Command::new("open")
                        .arg("-a")
                        .arg(path)
                        .arg("--args")
                        .args(args)
                        .status()?;
                    return Ok(if status.success() { 0 } else { 1 });
                }
            }
            let executable = installation.command(&package, command.as_deref())?;
            drop(guard);
            let status = std::process::Command::new(executable).args(args).status()?;
            return Ok(status
                .code()
                .and_then(|c| u8::try_from(c).ok())
                .unwrap_or(1));
        }
        Command::Path { package } => println!("{}", installation.path(&package)?.display()),
        Command::Omarchy { remove } => {
            ensure!(
                cfg!(target_os = "linux"),
                "Omarchy integration requires Linux"
            );
            let mut process = std::process::Command::new("python3");
            process
                .arg("-c")
                .arg(include_str!("integrations/omarchy.py"))
                .arg(std::env::current_exe()?)
                .arg(&home);
            if remove {
                process.arg("--remove");
            }
            ensure!(
                process.status()?.success(),
                "Omarchy menu integration failed"
            );
        }
        Command::List { json } => {
            let mut releases = Vec::new();
            for (id, receipt) in installation.receipts()? {
                let release = Release::verify(receipt.current)?;
                if json {
                    releases.push(release_json(&release, &trust, &config.aliases));
                } else {
                    println!(
                        "{id}\t{}\t{}",
                        release.data.package.version, release.event.id
                    );
                }
            }
            if json {
                println!("{}", serde_json::to_string(&releases)?);
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
            let public_key = nostr::PublicKey::parse(&resolve_key(&config, &public_key)?)?;
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
            release,
            version,
            target: requested_target,
            note,
            revoke,
            out,
            json,
        } => {
            let keys = own_keys(&home, &config)?;
            ensure!(
                !note.trim().is_empty(),
                "describe what you checked with --note"
            );
            let release_id = if let Ok(id) = EventId::from_hex(&release) {
                ensure!(
                    version.is_none(),
                    "--version is only used with a package name"
                );
                id
            } else {
                let version = version.context(
                    "use publisher/package --version VERSION, or an exact release event ID",
                )?;
                let package = resolve_package(&config, &release)?;
                let candidate = select(
                    candidates(&home, &mut config, None).await?,
                    &package,
                    Some(&version),
                    &config.aliases,
                    &trust,
                    requested_target.as_deref().unwrap_or(target()),
                    non_interactive,
                )?;
                if !json {
                    eprintln!(
                        "{} {} {} ({})",
                        if revoke {
                            "Revoking attestation for"
                        } else {
                            "Attesting to"
                        },
                        release_label(&candidate.release, &config.aliases),
                        version,
                        candidate.release.data.package.target
                    );
                    eprintln!("  Release: {}", candidate.release.event.id);
                    eprintln!("  Claim: {}", terminal_text(&note));
                }
                candidate.release.event.id
            };
            let mut event = attest(&keys, release_id, !revoke, note)?;
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
            let out =
                out.unwrap_or_else(|| home.join("attestations").join(format!("{}.json", event.id)));
            atomic_write(&out, &serde_json::to_vec_pretty(&event)?)?;
            if json {
                println!("{}", serde_json::to_string(&event)?);
            } else {
                println!("{}", event.id);
                eprintln!("Saved signed attestation: {}", out.display());
            }
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
