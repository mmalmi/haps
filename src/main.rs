use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use haps::comments::{self, Comments, Scope};
use haps::{
    discovery::{Announcement, Discovery},
    install::{Installation, ensure_public_key, load_keys},
    model::*,
    repository::{Repository, Snapshot},
    trust::{Trust, attest, attest_at, attestation_time_ms, parse_attestation, warn, warn_at},
};
use nostr::{Event, EventBuilder, EventId, Keys, Kind, Tag, Timestamp};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

#[derive(Parser)]
#[command(version, about = "Haps — Hashtree Package System")]
struct Cli {
    #[arg(long, global = true, env = "HAPS_HOME")]
    home: Option<PathBuf>,
    /// Start a fresh configuration without discovery presets or a trust seed.
    #[arg(long, global = true, env = "HAPS_NO_DEFAULTS")]
    no_defaults: bool,
    /// Never prompt; require publisher/name when installing a package.
    #[arg(long, global = true, env = "HAPS_NON_INTERACTIVE")]
    non_interactive: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct ClaimArgs {
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
    /// Explicitly record a completed audit of this exact release.
    #[arg(long, requires = "provenance", conflicts_with = "revoke")]
    audited: bool,
    /// State how the binary relates to reviewed source, including unverified claims.
    #[arg(long, requires = "audited")]
    provenance: Option<String>,
    /// Withdraw your previous claim without endorsing or warning.
    #[arg(long)]
    revoke: bool,
    #[arg(long)]
    out: Option<PathBuf>,
    /// Emit the signed event as JSON and never prompt.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Default)]
struct AuditReviewArgs {
    /// Reviewer: codex, claude, or manual (defaults to your audit settings).
    #[arg(long)]
    audit_agent: Option<String>,
    /// Request a model and record it in the signed audit evidence.
    #[arg(long)]
    audit_model: Option<String>,
    /// Publish this audit, overriding the saved preference for this operation.
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    publish_audit: Option<bool>,
}

#[derive(Subcommand)]
enum AuditAction {
    /// Clone pinned source to a private directory for a person or external agent.
    Prepare {
        package: String,
        #[arg(long)]
        version: Option<semver::Version>,
    },
    /// Approve your completed source review, rebuild, and require a binary match.
    Finish {
        session: String,
        #[arg(long)]
        note: String,
        /// Name of the external reviewer, recorded as user-supplied metadata.
        #[arg(long, default_value = "manual")]
        reviewer: String,
        /// Model used by the external reviewer (user-supplied, not verified).
        #[arg(long)]
        model: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        publish: Option<bool>,
    },
    /// Show or set review and publication defaults. Use --model default to clear it.
    Settings {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        publish: Option<bool>,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Prepare an external audit, complete it, or configure audit defaults.
    Audit {
        #[command(subcommand)]
        action: AuditAction,
    },
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
    /// Add a staged package or another publisher's package to your event catalog.
    Add {
        package: String,
        /// With a manifest path, sign a release from this staged directory.
        #[arg(long)]
        payload: Option<PathBuf>,
        #[arg(long, default_value = "default")]
        catalog: String,
    },
    /// Maintain and discover searchable catalogs of signed package events.
    Catalog {
        #[command(subcommand)]
        action: CatalogAction,
    },
    /// Sign staged files and prepare your package for publication.
    Pack {
        manifest: PathBuf,
        #[arg(long)]
        payload: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        key_file: Option<PathBuf>,
    },
    /// Prepare or publish packages from a verified Hashtree/Iris Git release directory.
    ImportRelease(haps::release::ReleaseArgs),
    /// Share signed packages through Hashtree and announce them on Nostr.
    Publish {
        #[arg(value_name = "DIRECTORY")]
        catalog: PathBuf,
        /// Hashtree name to create or update under your hosting identity.
        #[arg(long)]
        name: String,
        /// Catalog publisher's signing key (defaults to the local Haps identity).
        #[arg(long)]
        key_file: Option<PathBuf>,
    },
    /// Retry queued announcements and refresh the private discovery cache.
    Sync,
    /// Configure shared signed indexes or build one from the local event cache.
    #[command(hide = true)]
    Index {
        #[command(subcommand)]
        action: IndexAction,
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
        #[command(flatten)]
        audit_review: AuditReviewArgs,
        /// Share the passing source scan after a successful build and installation.
        #[arg(long, requires = "install")]
        attest_scan: bool,
    },
    /// Advanced compatibility controls for package sources.
    #[command(hide = true)]
    Source {
        #[command(subcommand)]
        action: SourceAction,
    },
    /// Search packages authored or vouched for by your social graph.
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
    /// Install a package. An explicit publisher/name can bypass the social filter.
    Install {
        package: String,
        #[arg(long)]
        version: Option<semver::Version>,
        #[arg(long)]
        allow_untrusted: bool,
        /// Override trusted release warnings for this operation only.
        #[arg(long)]
        allow_warnings: bool,
        /// Require this many explicit audits (at least one unless explicitly bypassed).
        #[arg(long, default_value_t = 1)]
        require_attestations: usize,
        /// Bypass the audit threshold for this operation only; preserve the saved policy.
        #[arg(long, conflicts_with_all = ["audit", "audit_agent", "audit_model", "publish_audit"])]
        allow_unaudited: bool,
        /// Review source, run its build with your user permissions, and require a matching payload.
        #[arg(long)]
        audit: bool,
        #[command(flatten)]
        audit_review: AuditReviewArgs,
        /// Emit JSON and never prompt.
        #[arg(long)]
        json: bool,
    },
    /// Update an installed package while retaining its publisher and attestation policy.
    Update {
        package: String,
        #[arg(long)]
        allow_untrusted: bool,
        /// Override trusted release warnings for this operation only.
        #[arg(long)]
        allow_warnings: bool,
        /// Bypass the audit threshold for this operation only; preserve the saved policy.
        #[arg(long, conflicts_with_all = ["audit", "audit_agent", "audit_model", "publish_audit"])]
        allow_unaudited: bool,
        #[arg(long)]
        audit: bool,
        #[command(flatten)]
        audit_review: AuditReviewArgs,
        /// Emit JSON and never prompt.
        #[arg(long)]
        json: bool,
    },
    /// Link installed commands into a directory; links follow updates and rollback.
    Link {
        package: String,
        #[arg(long)]
        bin_dir: Option<PathBuf>,
    },
    /// Configure automatic local scans before executing source builds.
    Security {
        /// Enable scans using a local Semgrep rules file; its contents are pinned.
        #[arg(long, conflicts_with = "disable")]
        rules: Option<PathBuf>,
        /// Semgrep executable (defaults to semgrep on PATH).
        #[arg(long, requires = "rules")]
        scanner: Option<PathBuf>,
        /// Disable the scan requirement, retaining private reports.
        #[arg(long)]
        disable: bool,
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
    #[cfg(target_os = "linux")]
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
    Rollback {
        package: String,
        /// Bypass the audit threshold for this operation only; preserve the saved policy.
        #[arg(long)]
        allow_unaudited: bool,
    },
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
    /// Publish a signed endorsement of an exact release.
    Attest(ClaimArgs),
    /// Publish a signed warning about an exact release.
    Warn(ClaimArgs),
    /// Publish a signed package comment, release comment, or reply.
    Comment {
        package: String,
        text: String,
        /// Target an exact release event ID instead of the whole package.
        #[arg(long)]
        release: Option<String>,
        #[arg(long)]
        reply_to: Option<String>,
        /// Also export the signed comment to a file.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Find comments, with social context and local mutes applied.
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
            | Self::List { json } => *json,
            Self::Attest(args) | Self::Warn(args) => args.json,
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
enum CatalogAction {
    /// List your catalogs and the indexes searched by Haps.
    List,
    /// Show the original signed events in one of your catalogs.
    Show {
        #[arg(default_value = "default")]
        name: String,
    },
    /// Remove a package from your catalog; installed packages are unaffected.
    Remove {
        package: String,
        #[arg(long, default_value = "default")]
        catalog: String,
    },
    /// Include a published event index in searches.
    Add {
        name: String,
        location: String,
        #[arg(long)]
        author: String,
    },
    /// Find event catalogs published by people in your social graph.
    Discover,
    /// Upload your catalog and announce its immutable index on Nostr.
    Publish {
        #[arg(default_value = "default")]
        name: String,
    },
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
enum IndexAction {
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
    /// Collect signed announcements and export a shareable Hashtree index.
    Build {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        key_file: Option<PathBuf>,
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
    #[serde(default)]
    indexes: BTreeMap<String, Source>,
    #[serde(skip)]
    aliases: BTreeMap<String, String>,
    #[serde(default)]
    starting_point: Option<String>,
    #[serde(default)]
    discovery_defaults_version: u32,
    #[serde(default)]
    audit: AuditSettings,
}

#[derive(Default, Serialize, Deserialize)]
struct AuditSettings {
    agent: Option<String>,
    model: Option<String>,
    publish: Option<bool>,
}

const DISCOVERY_DEFAULTS_VERSION: u32 = 2;
const LEGACY_INDEX_LOCATION: &str =
    "htree://npub1q6g6t3yk0m2ppp5mrqsze4xg6uqhw5p29kjutet5m3uk637xjfaqac3p2a/package-index";
const LEGACY_INDEX_AUTHOR: &str =
    "731fd6f74667cac0e86b7b4f7cd2c828996db866c3044368a2f26d87cb571ad0";

fn discovery_presets(config: &mut Config) -> Result<()> {
    // A signed immutable bootstrap makes a cold install independent of mutable
    // root availability. Social root announcements can add newer indexes later.
    let event: Event = serde_json::from_str(include_str!("../packages/catalog-root.json"))?;
    let announcement = haps::event_catalog::IndexAnnouncement::verify(event)?;
    config
        .indexes
        .entry("haps".into())
        .or_insert_with(|| Source {
            location: announcement.location,
            author: announcement.event.pubkey.to_hex(),
            sequence: 0,
            event_id: announcement.event.id.to_hex(),
        });
    Ok(())
}

fn fresh_config(no_defaults: bool) -> Result<Config> {
    let mut config = Config {
        discovery_defaults_version: DISCOVERY_DEFAULTS_VERSION,
        ..Default::default()
    };
    if !no_defaults {
        discovery_presets(&mut config)?;
        let npub = hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB;
        let key = ensure_public_key(npub)?;
        config.starting_point = Some(key.clone());
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
    repository: Arc<Repository>,
    package_card: Option<String>,
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

fn audit_agent(selected: Option<&str>, interactive: bool) -> Result<String> {
    if let Some(agent) = selected {
        ensure!(
            matches!(agent, "codex" | "claude" | "manual"),
            "supported audit agents: codex, claude, manual"
        );
        return Ok(agent.into());
    }
    ensure!(
        interactive,
        "choose --audit-agent codex or claude for a non-interactive audit; install and sign in to the chosen agent first"
    );
    let agents = haps::audit::agents().unwrap_or_else(|_| {
        eprintln!("Agent detection needs Python 3; manual review is still available.");
        vec!["manual".into()]
    });
    let labels: Vec<_> = agents
        .iter()
        .map(|agent| {
            if agent == "manual" {
                "Review it myself".to_owned()
            } else {
                format!("Review with {agent}")
            }
        })
        .collect();
    let choice = dialoguer::Select::new()
        .with_prompt("Choose how to audit")
        .items(&labels)
        .default(0)
        .interact_opt()?
        .context("audit cancelled")?;
    Ok(agents[choice].clone())
}

async fn record_audit(
    home: &Path,
    config: &mut Config,
    trust: &mut Trust,
    keys: &Keys,
    release: &Release,
    report: (&haps::audit::Evidence, String),
    publication: (Option<bool>, bool),
) -> Result<()> {
    let (evidence, note) = report;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let mut at = u64::try_from(now)?;
    for event in trust.events().iter().filter(|event| {
        event.pubkey == keys.public_key()
            && parse_attestation(event)
                .is_ok_and(|claim| claim.release == release.event.id.to_hex())
    }) {
        at = at.max(
            (attestation_time_ms(event)? / 1000)
                .checked_add(1)
                .and_then(|t| t.checked_mul(1000))
                .context("audit timestamp overflow")?,
        );
    }
    let event = haps::trust::attest_audit_at(keys, release.event.id, note, evidence, at)?;
    trust.ingest(event.clone())?;
    save_trust(home, trust)?;
    atomic_write(
        &home.join("audits").join(format!("{}.json", event.id)),
        &serde_json::to_vec_pretty(&event)?,
    )?;
    eprintln!("Audit: {}", terminal_text(&parse_attestation(&event)?.note));
    eprintln!("Provenance: {}", terminal_text(&evidence.provenance));
    if let Some(reviewer) = &evidence.reviewer {
        eprintln!(
            "Reviewer: {} | requested model: {} | reported model: {}",
            terminal_text(&reviewer.agent),
            terminal_text(
                reviewer
                    .requested_model
                    .as_deref()
                    .unwrap_or("agent default")
            ),
            terminal_text(reviewer.reported_model.as_deref().unwrap_or("not reported"))
        );
    }
    let publish = if let Some(value) = publication.0.or(config.audit.publish) {
        value
    } else if publication.1 {
        let value = dialoguer::Confirm::new()
            .with_prompt("Publish this and future audits (signed note, provenance, agent/model) to your configured relays?")
            .default(false).interact()?;
        config.audit.publish = Some(value);
        save_config(home, config)?;
        value
    } else {
        false
    };
    if publish {
        publish_feedback(home, config, &event).await?;
        eprintln!("Audit saved and queued for publication.");
    } else {
        eprintln!(
            "Audit saved locally. Change the default with haps audit settings --publish true."
        );
    }
    Ok(())
}

struct AuditOptions<'a> {
    requested: bool,
    review: &'a AuditReviewArgs,
    non_interactive: bool,
    allow_untrusted: bool,
    allow_warnings: bool,
    allow_unaudited: bool,
    minimum: usize,
}

async fn ensure_audited(
    home: &Path,
    config: &mut Config,
    trust: &mut Trust,
    candidate: &Candidate,
    options: AuditOptions<'_>,
) -> Result<()> {
    use std::io::IsTerminal;
    let release = &candidate.release;
    trust.authorize_with_policy(release, options.allow_untrusted, 0, options.allow_warnings)?;
    if options.allow_unaudited {
        eprintln!("Warning: bypassing the audit requirement for this operation only.");
        return Ok(());
    }
    let enough = trust.audits(release).len() >= options.minimum.max(1);
    let requested = options.requested
        || options.review.audit_agent.is_some()
        || options.review.audit_model.is_some();
    ensure!(
        requested || options.review.publish_audit.is_none(),
        "--publish-audit requires --audit or --audit-agent"
    );
    if enough && !requested {
        return Ok(());
    }
    if release.data.package.source.is_none() {
        ensure!(
            !requested,
            "Audit and install requires pinned source. A completed manual audit can be recorded with its provenance"
        );
        return trust.authorize_install(release, options.allow_untrusted, options.minimum, options.allow_warnings)
            .context("Audit and install requires pinned source. A completed manual audit can be recorded with its provenance");
    }
    let interactive = !options.non_interactive
        && std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal();
    if !requested && (!interactive || !dialoguer::Confirm::new()
            .with_prompt("This release needs an audit. Audit and install (review source, run the build recipe, and require a binary match)?")
            .default(false).interact()?) {
            return trust.authorize_install(release, options.allow_untrusted, options.minimum, options.allow_warnings);
    }
    let agent = audit_agent(
        options
            .review
            .audit_agent
            .as_deref()
            .or(config.audit.agent.as_deref()),
        interactive,
    )?;
    let model = options.review.audit_model.clone().or_else(|| {
        (agent != "manual")
            .then(|| config.audit.model.clone())
            .flatten()
    });
    // An explicit choice to audit locally also creates an identity if needed.
    let keys = publishing_keys(home, config)?;
    *trust = load_trust(home, config)?;
    let other_audits = trust
        .audits(release)
        .iter()
        .filter(|e| e.pubkey != keys.public_key())
        .count();
    ensure!(
        other_audits + 1 >= options.minimum.max(1),
        "a self-audit adds one reviewer; more independent audits are required by this installation's policy"
    );
    let (evidence, note) = haps::audit::review_and_rebuild(
        home,
        release,
        &candidate.repository,
        &agent,
        model.as_deref(),
        interactive,
    )
    .await?;
    record_audit(
        home,
        config,
        trust,
        &keys,
        release,
        (&evidence, note),
        (options.review.publish_audit, interactive),
    )
    .await?;
    trust.authorize_install(
        release,
        options.allow_untrusted,
        options.minimum,
        options.allow_warnings,
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

fn publishing_keys(home: &Path, config: &mut Config) -> Result<Keys> {
    if config.identity.is_none() && !home.join("identity.key").exists() {
        use std::io::Write;
        let keys = Keys::generate();
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(home.join("identity.key"))?;
        file.write_all(keys.secret_key().to_secret_hex().as_bytes())?;
        file.sync_all()?;
        config.identity = Some(keys.public_key().to_hex());
        save_config(home, config)?;
    }
    own_keys(home, config)
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

async fn direct_candidates(
    home: &Path,
    query: Option<&str>,
    package: Option<&str>,
) -> Result<Vec<Candidate>> {
    let discovery = Discovery::open(home)?;
    let announcements = discovery.announcements(None).await?;
    let cached: BTreeMap<_, _> = discovery
        .events(vec![nostr::Filter::new().kind(APP_KIND)])
        .await?
        .into_iter()
        .map(|event| (event.id.to_hex(), event))
        .collect();
    drop(discovery);
    let mut found = Vec::new();
    for announcement in announcements {
        let Some(ref head) = announcement.head else {
            continue;
        };
        let author = announcement.author();
        let identity = format!("{author}/{}", announcement.name);
        if package.is_some_and(|p| p != identity && p != announcement.name) {
            continue;
        }
        if let Some(query) = query {
            let text = format!("{} {}", announcement.name, head.description).to_lowercase();
            if !query
                .split_whitespace()
                .all(|term| text.contains(&term.to_lowercase()))
            {
                continue;
            }
        }
        let result: Result<Vec<Candidate>> = async {
            let repo = Arc::new(Repository::open(&head.payload, &home.join("cache"))?);
            let mut releases = Vec::new();
            for pointer in &head.releases {
                let event: Event = match cached.get(&pointer.id) {
                    Some(event) => event.clone(),
                    None => repo.json(&pointer.cid).await?,
                };
                let release = head.verify_release(pointer, event, &author, &announcement.name)?;
                releases.push(Candidate {
                    repository: repo.clone(),
                    package_card: Some(head.card.clone()),
                    release,
                });
            }
            Ok(releases)
        }
        .await;
        match result {
            Ok(releases) => found.extend(releases),
            Err(error) => eprintln!(
                "Package {identity} unavailable or invalid; results may be incomplete: {error:#}"
            ),
        }
    }
    Ok(found)
}

async fn candidates(
    home: &Path,
    config: &mut Config,
    query: Option<&str>,
    package: Option<&str>,
) -> Result<Vec<Candidate>> {
    let mut found = direct_candidates(home, query, package).await?;
    let mut available = usize::from(!found.is_empty());
    let mut seen: BTreeMap<_, _> = found
        .iter()
        .map(|c| (c.release.coordinate(), c.release.event.id))
        .collect();
    let direct_authors: BTreeSet<_> = found.iter().map(|c| c.release.author()).collect();
    for (name, source) in &mut config.sources {
        if package.is_some() && direct_authors.contains(&source.author) {
            continue;
        }
        let repo = match Repository::open(&source.location, &home.join("cache")) {
            Ok(repo) => Arc::new(repo),
            Err(error) => {
                eprintln!(
                    "Package source {name} unavailable; results may be incomplete: {error:#}"
                );
                continue;
            }
        };
        let snapshot = match repo.catalog(&source.author).await {
            Ok(snapshot) => Arc::new(snapshot),
            Err(error) => {
                eprintln!("Package source {name} unavailable or invalid: {error:#}");
                continue;
            }
        };
        check_checkpoint(source, &snapshot)?;
        available += 1;
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
                repository: repo.clone(),
                package_card: snapshot.catalog.packages.get(&release.identity()).cloned(),
                release,
            });
        }
    }
    save_config(home, config)?;
    ensure!(
        available > 0,
        "package discovery is unavailable or incomplete. Check your connection and try haps sync"
    );
    Ok(found)
}

struct Selection {
    non_interactive: bool,
    include_untrusted: bool,
    confirm_install: bool,
}

fn select(
    mut candidates: Vec<Candidate>,
    package: &str,
    version: Option<&semver::Version>,
    aliases: &BTreeMap<String, String>,
    trust: &Trust,
    target_filter: &str,
    selection: Selection,
) -> Result<Candidate> {
    use std::io::IsTerminal;
    let interactive = !selection.non_interactive
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
        && !std::env::var("TERM").is_ok_and(|term| term == "dumb");
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
    // Choose the newest matching version before trust filtering. An endorsement
    // of an older release must never silently pin or downgrade a bare install.
    let mut latest = BTreeMap::new();
    for candidate in &candidates {
        let version = &candidate.release.data.package.version;
        let current = latest.entry(candidate.release.author()).or_insert(version);
        *current = (*current).max(version);
    }
    let latest: BTreeMap<_, _> = latest.into_iter().map(|(k, v)| (k, v.clone())).collect();
    candidates.retain(|c| latest[&c.release.author()] == c.release.data.package.version);
    if !selection.include_untrusted && !package.contains('/') {
        candidates.retain(|c| trust.socially_trusted(&c.release));
    }
    let authors: BTreeSet<_> = candidates.iter().map(|c| c.release.author()).collect();
    let confirm_single = selection.confirm_install && !package.contains('/') && authors.len() == 1;
    ensure!(
        !candidates.is_empty(),
        "no matching release for {} in the selected scope; packages outside your social graph require an explicit npub/package",
        target_filter
    );
    if authors.len() > 1 {
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
        if !interactive {
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
    let candidate = candidates.remove(0);
    if confirm_single {
        let release = &candidate.release;
        if !interactive {
            return Err(PublisherConfirmationRequired {
                candidate: release_json(release, trust, aliases),
            }
            .into());
        }
        use nostr::nips::nip19::ToBech32;
        eprintln!(
            "Found {} {} · {}",
            release_label(release, aliases),
            release.data.package.version,
            relationship(trust, &release.author(), aliases),
        );
        eprintln!("Publisher: {}", release.event.pubkey.to_bech32()?);
        eprintln!("  {}", attestation_summary(release, trust, aliases));
        ensure!(
            dialoguer::Confirm::new()
                .with_prompt("Install this package?")
                .default(false)
                .interact()?,
            "cancelled; nothing was installed"
        );
    }
    Ok(candidate)
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
    let mut summary = if names.is_empty() {
        "No trusted attestations".into()
    } else {
        format!("Attested by {}", names.join(", "))
    };
    summary.push_str(&format!(
        "; {} trusted audit(s)",
        trust.audits(release).len()
    ));
    let mut warning_names: Vec<_> = trust
        .warnings(release)
        .iter()
        .map(|event| terminal_text(&publisher_label(aliases, &event.pubkey.to_hex())))
        .collect();
    warning_names.sort();
    if !warning_names.is_empty() {
        summary.push_str(&format!("; Warning from {}", warning_names.join(", ")));
    }
    summary
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
    let warnings: Vec<_> = trust
        .warnings(release)
        .iter()
        .map(|event| {
            serde_json::json!({"signer": event.pubkey.to_hex(),
            "label": publisher_label(aliases, &event.pubkey.to_hex()), "event": event})
        })
        .collect();
    let audits: Vec<_> = trust
        .audits(release)
        .iter()
        .map(|event| {
            serde_json::json!({"signer": event.pubkey.to_hex(),
            "label": publisher_label(aliases, &event.pubkey.to_hex()),
            "evidence": haps::trust::parse_audit(event).expect("validated audit"), "event": event})
        })
        .collect();
    serde_json::json!({
        "identity": release.identity(),
        "label": release_label(release, aliases),
        "package": release.data.package, "publisher": release.author(),
        "release_id": release.event.id.to_hex(), "manifest": release.data.manifest,
        "follow_distance": trust.distance(&release.author()), "muted": trust.muted(&release.author()),
        "overmuted": trust.overmuted(&release.author()), "socially_trusted": trust.socially_trusted(release),
        "followed_by": trust.followed_by_friends(&release.author()).iter().map(|key| serde_json::json!({"pubkey": key, "label": publisher_label(aliases, key)})).collect::<Vec<_>>(),
        "attesters": trust.attesters(release), "attestations": attestations,
        "warnings": warnings, "audits": audits, "minimum_audits": 1,
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

#[derive(Debug)]
struct PublisherConfirmationRequired {
    candidate: serde_json::Value,
}

impl std::fmt::Display for PublisherConfirmationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "bare-name installation needs publisher confirmation; use an interactive terminal or specify publisher/name:\n  haps install {}",
            self.candidate["identity"].as_str().unwrap()
        )
    }
}
impl std::error::Error for PublisherConfirmationRequired {}

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
    for event in trust
        .attestations(release)
        .into_iter()
        .chain(trust.warnings(release))
    {
        if let Ok(claim) = parse_attestation(event) {
            eprintln!(
                "    {}: {}",
                terminal_text(&publisher_label(aliases, &event.pubkey.to_hex())),
                terminal_text(&claim.note)
            );
            if let Ok(evidence) = haps::trust::parse_audit(event) {
                eprintln!(
                    "      Binary match: {}. {}",
                    if evidence.binary_match {
                        "verified by auditor"
                    } else {
                        "unverified"
                    },
                    terminal_text(&evidence.provenance)
                );
                if let Some(reviewer) = evidence.reviewer {
                    eprintln!(
                        "      {} · requested model: {} · reported model: {}",
                        terminal_text(&reviewer.agent),
                        terminal_text(reviewer.requested_model.as_deref().unwrap_or("default")),
                        terminal_text(reviewer.reported_model.as_deref().unwrap_or("not reported"))
                    );
                }
            }
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
    // The dispatcher contains every command's future. Keep it off the native
    // entry-point stack, which is only 1 MiB by default on Windows.
    match Box::pin(execute(cli)).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            if json {
                let mut value = serde_json::json!({"code": "operation_failed", "message": format!("{error:#}")});
                if let Some(ambiguous) = error.downcast_ref::<AmbiguousPublishers>() {
                    value["code"] = "ambiguous_package".into();
                    value["candidates"] = serde_json::json!(ambiguous.candidates);
                }
                if let Some(unconfirmed) = error.downcast_ref::<PublisherConfirmationRequired>() {
                    value["code"] = "publisher_confirmation_required".into();
                    value["candidate"] = unconfirmed.candidate.clone();
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
    let (progress, _reporter) = haps::progress::Progress::stderr(
        !cli.command.json()
            && matches!(
                cli.command,
                Command::Install { .. } | Command::Update { .. }
            ),
    )?;
    match &cli.command {
        Command::Install { package, .. } => {
            progress.stage(format!("Preparing to install {}", terminal_text(package)));
        }
        Command::Update { package, .. } => {
            progress.stage(format!(
                "Checking for updates to {}",
                terminal_text(package)
            ));
        }
        _ => {}
    }
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
    let guard = lock_with_wait(&home.join(".cli.lock"), || {
        progress.stage("Waiting for another Haps command");
    })?;
    progress.stage("Loading saved social graph");
    let config_file = home.join("config.json");
    let mut config: Config = if config_file.exists() {
        read_json(&config_file)?
    } else {
        fresh_config(cli.no_defaults)?
    };
    if config.discovery_defaults_version < DISCOVERY_DEFAULTS_VERSION {
        let legacy_index = config.indexes.get("haps").is_some_and(|source| {
            source.location == LEGACY_INDEX_LOCATION && source.author == LEGACY_INDEX_AUTHOR
        });
        let original_defaults = config.discovery_defaults_version == 0
            && (config.starting_point.is_some() || config.sources.contains_key("iris"));
        if !cli.no_defaults && (legacy_index || original_defaults) {
            if legacy_index {
                config.indexes.remove("haps");
            }
            discovery_presets(&mut config)?;
            let npub = hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB;
            let author = ensure_public_key(npub)?;
            // Replace only the exact old automatic source, retaining custom
            // source locations, publisher pins, and explicitly removed presets.
            if config.sources.get("iris").is_some_and(|source| {
                source.location == format!("htree://{npub}/haps-packages")
                    && source.author == author
            }) {
                config.sources.remove("iris");
            }
        }
        config.discovery_defaults_version = DISCOVERY_DEFAULTS_VERSION;
        save_config(&home, &config)?;
    }
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
        | Command::Rollback { package, .. }
        | Command::Remove { package }
        | Command::Comment { package, .. }
        | Command::Comments { package, .. } => {
            *package = resolve_package(&config, package)?;
        }
        _ => {}
    }
    match &cli.command {
        Command::Install { package, .. }
        | Command::Info { package, .. }
        | Command::Update { package, .. }
        | Command::Comment { package, .. }
        | Command::Comments { package, .. } => {
            let publisher = package
                .split_once('/')
                .map(|(key, _)| nostr::PublicKey::parse(key))
                .transpose()?;
            progress.stage("Looking up packages and social trust");
            refresh_discovery(
                &home,
                &mut config,
                &mut trust,
                publisher,
                Some(package.rsplit('/').next().unwrap()),
                None,
            )
            .await?;
        }
        _ => {}
    }
    let installation = Installation::new(home.clone())?.with_progress(progress.clone());
    let is_warning = matches!(&cli.command, Command::Warn(_));
    match cli.command {
        Command::Audit { action } => match action {
            AuditAction::Settings {
                agent,
                model,
                publish,
            } => {
                if let Some(agent) = agent {
                    config.audit.agent = Some(audit_agent(Some(&agent), false)?);
                }
                if let Some(model) = model {
                    ensure!(
                        !model.trim().is_empty()
                            && model.len() <= 200
                            && !model.chars().any(char::is_control),
                        "invalid audit model"
                    );
                    config.audit.model = (model != "default").then_some(model);
                }
                if publish.is_some() {
                    config.audit.publish = publish;
                }
                save_config(&home, &config)?;
                println!("{}", serde_json::to_string_pretty(&config.audit)?);
            }
            AuditAction::Prepare { package, version } => {
                let package = resolve_package(&config, &package)?;
                let choices = candidates(&home, &mut config, None, Some(&package)).await?;
                let releases = choices
                    .iter()
                    .map(|c| c.release.clone())
                    .collect::<Vec<_>>();
                refresh_feedback(&home, &config, &mut trust, &releases).await?;
                let candidate = select(
                    choices,
                    &package,
                    version.as_ref(),
                    &config.aliases,
                    &trust,
                    target(),
                    Selection {
                        non_interactive,
                        include_untrusted: package.contains('/'),
                        confirm_install: false,
                    },
                )?;
                let directory = haps::audit::prepare(&home, &candidate.release)?;
                println!("Source to review: {}", directory.join("repo").display());
                println!("Pinned release: {}", candidate.release.event.id);
                println!(
                    "This checkout is private, but it is not a sandbox. Preparation has not executed build commands. Review haps-build.toml and all source; keep the checkout unchanged."
                );
                println!(
                    "After review, run with the same Haps home: haps audit finish {} --note \"What you checked\"",
                    directory
                        .file_name()
                        .context("session name missing")?
                        .to_string_lossy()
                );
                println!(
                    "Finishing runs the recipe with your user permissions and requires the rebuilt payload to match. Then run haps install {} --version {}.",
                    release_label(&candidate.release, &config.aliases),
                    candidate.release.data.package.version
                );
            }
            AuditAction::Finish {
                session,
                note,
                reviewer,
                model,
                publish,
            } => {
                use std::io::IsTerminal;
                let interactive = !non_interactive
                    && std::io::stdin().is_terminal()
                    && std::io::stderr().is_terminal();
                let reviewer = haps::audit::Reviewer {
                    agent: reviewer,
                    requested_model: model,
                    reported_model: None,
                };
                // Validate metadata and signing capability before running a build.
                let mut validation = haps::audit::Evidence::manual("External review".into())?;
                validation.reviewer = Some(reviewer.clone());
                validation.validate()?;
                let keys = publishing_keys(&home, &mut config)?;
                trust = load_trust(&home, &config)?;
                let (release, evidence) =
                    haps::audit::finish(&home, &session, &note, reviewer).await?;
                record_audit(
                    &home,
                    &mut config,
                    &mut trust,
                    &keys,
                    &release,
                    (&evidence, note),
                    (publish, interactive),
                )
                .await?;
                println!(
                    "Audited {} ({}) with a matching rebuild. Ready to install.",
                    release_label(&release, &config.aliases),
                    release.event.id
                );
            }
        },
        Command::Target | Command::Init(_) => unreachable!(),
        Command::Add {
            package,
            payload,
            catalog,
        } => {
            let directory = haps::event_catalog::directory(&home, &catalog)?;
            if let Some(payload) = payload {
                let spec: PackageSpec = toml::from_str(&fs::read_to_string(&package)?)?;
                let keys = publishing_keys(&home, &mut config)?;
                let repo = Repository::local(directory.join("packages"))?;
                let release = repo.publish(&keys, spec, &payload).await?;
                Discovery::open(&directory)?.ingest([release.event]).await?;
            } else {
                let package = resolve_package(&config, &package)?;
                let publisher = package
                    .split_once('/')
                    .map(|(key, _)| nostr::PublicKey::parse(key))
                    .transpose()?;
                refresh_discovery(
                    &home,
                    &mut config,
                    &mut trust,
                    publisher,
                    Some(package.rsplit('/').next().unwrap()),
                    None,
                )
                .await?;
                haps::event_catalog::add_package(&home, &catalog, &package).await?;
            }
            println!("Added to catalog {catalog}. Publish with: haps catalog publish {catalog}");
        }
        Command::Catalog { action } => {
            use haps::event_catalog::{IndexAnnouncement, directory};
            match action {
                CatalogAction::Discover => {
                    refresh_discovery(&home, &mut config, &mut trust, None, None, None).await?;
                    println!("{}", serde_json::to_string_pretty(&config.indexes)?);
                }
                CatalogAction::List => {
                    let path = home.join("catalogs");
                    let mut own = Vec::new();
                    if path.exists() {
                        for entry in fs::read_dir(path)? {
                            let entry = entry?;
                            if entry.file_type()?.is_dir() {
                                own.push(entry.file_name().to_string_lossy().into_owned());
                            }
                        }
                    }
                    own.sort();
                    println!(
                        "{}",
                        serde_json::json!({"own": own, "indexes": config.indexes})
                    );
                }
                CatalogAction::Show { name } => {
                    let path = directory(&home, &name)?;
                    ensure!(path.exists(), "catalog does not exist");
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &Discovery::open(&path)?
                                .events(vec![nostr::Filter::new()])
                                .await?
                        )?
                    );
                }
                CatalogAction::Remove { package, catalog } => {
                    let package = resolve_package(&config, &package)?;
                    let path = directory(&home, &catalog)?;
                    ensure!(path.exists(), "catalog does not exist");
                    let selected = Discovery::open(&path)?;
                    let events = selected.events(vec![nostr::Filter::new()]).await?;
                    let identity = |event: &Event| -> Option<String> {
                        if let Ok(a) = Announcement::verify(event.clone()) {
                            Some(format!("{}/{}", a.author(), a.name))
                        } else {
                            Release::verify(event.clone()).ok().map(|r| r.identity())
                        }
                    };
                    let matches: BTreeSet<_> = events
                        .iter()
                        .filter_map(identity)
                        .filter(|id| {
                            *id == package || id.rsplit('/').next() == Some(package.as_str())
                        })
                        .collect();
                    ensure!(
                        matches.len() == 1,
                        "package is missing or ambiguous; use publisher/name"
                    );
                    selected
                        .replace_events(events.into_iter().filter(|event| {
                            !identity(event).is_some_and(|id| matches.contains(&id))
                        }))
                        .await?;
                    println!("Removed from catalog {catalog}");
                }
                CatalogAction::Add {
                    name,
                    location,
                    author,
                } => {
                    safe_name(&name)?;
                    ensure!(!config.indexes.contains_key(&name), "index already exists");
                    let author = resolve_key(&config, &author)?;
                    Discovery::open(&home)?
                        .import_index(&location, &author)
                        .await?;
                    config.indexes.insert(
                        name,
                        Source {
                            location,
                            author,
                            sequence: 0,
                            event_id: String::new(),
                        },
                    );
                    save_config(&home, &config)?;
                }
                CatalogAction::Publish { name } => {
                    let path = directory(&home, &name)?;
                    ensure!(path.exists(), "catalog does not exist; add a package first");
                    let keys = publishing_keys(&home, &mut config)?;
                    let selected = Discovery::open(&path)?;
                    // Local package records are built once; public indexes retain
                    // original signatures for packages curated from elsewhere.
                    let selected_events = selected.events(vec![nostr::Filter::new()]).await?;
                    let selected_ids: BTreeSet<_> = selected_events
                        .iter()
                        .filter_map(|e| Release::verify(e.clone()).ok().map(|r| r.identity()))
                        .collect();
                    let mut package_events = Vec::new();
                    if path.join("packages/catalog.json").exists() {
                        package_events = haps::event_catalog::package_events(
                            &home,
                            &path.join("packages"),
                            &keys,
                            None,
                        )
                        .await?;
                        package_events.retain(|event| {
                            if let Ok(a) = Announcement::verify(event.clone()) {
                                selected_ids.contains(&format!("{}/{}", a.author(), a.name))
                            } else {
                                Release::verify(event.clone())
                                    .is_ok_and(|r| selected_ids.contains(&r.identity()))
                            }
                        });
                        selected.ingest(package_events.clone()).await?;
                    }
                    let location = haps::event_catalog::upload_index(
                        &selected.events(vec![nostr::Filter::new()]).await?,
                    )?;
                    drop(selected);
                    let discovery = Discovery::open(&home)?;
                    let previous = discovery.events(vec![nostr::Filter::new()]).await?;
                    let announcement = haps::event_catalog::advance(
                        &keys,
                        IndexAnnouncement::sign(&keys, &name, &location)?,
                        &previous,
                    )?;
                    package_events.push(announcement);
                    discovery.queue(&package_events)?;
                    discovery.ingest(package_events).await?;
                    flush_discovery(&home, &discovery).await;
                    println!("Published catalog {name}: {location}");
                }
            }
        }
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
        Command::Link { package, bin_dir } => {
            let package = resolve_package(&config, &package)?;
            let directory = installation.link(&package, bin_dir.as_deref())?;
            println!(
                "Linked commands in {}. Add this directory to PATH before other installations.",
                directory.display()
            );
        }
        Command::Security {
            rules,
            scanner,
            disable,
        } => {
            haps::security::configure(&home, rules.as_deref(), scanner.as_deref(), disable)?;
        }
        Command::ImportRelease(args) => {
            if let Some((path, name, keys)) = haps::release::prepare(&args, &home).await? {
                if args.publish {
                    let mut events =
                        haps::event_catalog::package_events(&home, &path, &keys, None).await?;
                    let location = haps::event_catalog::upload_index(&events)?;
                    let discovery = Discovery::open(&home)?;
                    let previous = discovery.events(vec![nostr::Filter::new()]).await?;
                    events.push(haps::event_catalog::advance(
                        &keys,
                        haps::event_catalog::IndexAnnouncement::sign(&keys, &name, &location)?,
                        &previous,
                    )?);
                    discovery.queue(&events)?;
                    discovery.ingest(events).await?;
                    let bus = haps::discovery::relay_bus(&home).await?;
                    ensure!(
                        discovery.flush(&bus).await? == 0,
                        "release announcements remain queued; run haps sync before considering publication complete"
                    );
                    println!("Release announcements acknowledged by a relay");
                } else {
                    println!("Prepared catalog {name}; publish with haps catalog publish {name}");
                }
            }
        }
        Command::Publish {
            catalog,
            name,
            key_file,
        } => {
            let path = catalog
                .canonicalize()
                .context("catalog directory is missing")?;
            let keys = load_keys(&key_file.unwrap_or_else(|| home.join("identity.key")))?;
            let events =
                haps::event_catalog::package_events(&home, &path, &keys, Some(&name)).await?;
            let discovery = Discovery::open(&home)?;
            discovery.queue(&events)?;
            discovery.ingest(events).await?;
            flush_discovery(&home, &discovery).await;
        }
        Command::Sync => {
            let discovery = Discovery::open(&home)?;
            flush_discovery(&home, &discovery).await;
            drop(discovery);
            refresh_discovery(&home, &mut config, &mut trust, None, None, None).await?;
        }
        Command::Index { action } => match action {
            IndexAction::Add {
                name,
                location,
                author,
            } => {
                safe_name(&name)?;
                let author = resolve_key(&config, &author)?;
                Discovery::open(&home)?
                    .import_index(&location, &author)
                    .await?;
                config.indexes.insert(
                    name,
                    Source {
                        location,
                        author,
                        sequence: 0,
                        event_id: String::new(),
                    },
                );
                save_config(&home, &config)?;
            }
            IndexAction::Remove { name } => {
                ensure!(config.indexes.remove(&name).is_some(), "index not found");
                save_config(&home, &config)?;
            }
            IndexAction::List => println!("{}", serde_json::to_string_pretty(&config.indexes)?),
            IndexAction::Build { out, key_file } => {
                refresh_discovery(&home, &mut config, &mut trust, None, None, None).await?;
                let keys = load_keys(&key_file.unwrap_or_else(|| home.join("identity.key")))?;
                Discovery::open(&home)?.export_index(&out, &keys).await?;
                println!(
                    "Built signed index in {}. Publish with htree add <directory> --publish <name>.",
                    out.display()
                );
            }
        },
        Command::Build {
            repository,
            rev,
            recipe,
            execute,
            install,
            audit_review,
            attest_scan,
        } => {
            ensure!(
                install
                    || (audit_review.audit_agent.is_none()
                        && audit_review.audit_model.is_none()
                        && audit_review.publish_audit.is_none()),
                "audit options for build require --install"
            );
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
                let reviewed = if install {
                    use std::io::IsTerminal;
                    let interactive = !non_interactive
                        && std::io::stdin().is_terminal()
                        && std::io::stderr().is_terminal();
                    let agent = audit_agent(
                        audit_review
                            .audit_agent
                            .as_deref()
                            .or(config.audit.agent.as_deref()),
                        interactive,
                    )?;
                    let model = audit_review.audit_model.as_deref().or_else(|| {
                        (agent != "manual")
                            .then_some(config.audit.model.as_deref())
                            .flatten()
                    });
                    Some((
                        haps::audit::review(&checkout, &agent, model, interactive)?,
                        interactive,
                    ))
                } else {
                    None
                };
                let scan = if haps::security::enabled(&home) {
                    Some(haps::security::scan_source(
                        &home,
                        checkout
                            .recipe
                            .package
                            .source
                            .as_ref()
                            .context("source is missing")?,
                        &checkout.root(),
                    )?)
                } else {
                    ensure!(
                        !attest_scan,
                        "enable source scans with haps security --rules FILE first"
                    );
                    None
                };
                let payload = checkout.execute()?;
                let repo = Repository::local(home.join("built-packages"))?;
                let release = repo
                    .publish(&keys, checkout.recipe.package.clone(), &payload)
                    .await?;
                if install {
                    let (review, interactive) = reviewed.context("source review missing")?;
                    let evidence = haps::audit::Evidence {
                        schema: "haps.audit.v1".into(), method: format!("{}-review-local-build", review.reviewer.agent),
                        provenance: "Built locally from reviewed pinned source and recipe. No publisher binary was compared; dependencies and toolchain are not independently verified.".into(),
                        binary_match: false,
                        reviewer: Some(review.reviewer),
                    };
                    record_audit(
                        &home,
                        &mut config,
                        &mut trust,
                        &keys,
                        &release,
                        (&evidence, review.note),
                        (audit_review.publish_audit, interactive),
                    )
                    .await?;
                    trust.authorize_install(&release, true, 1, false)?;
                    installation.install_with_policy(&repo, &release, 1).await?;
                }
                if let Some(report) = scan {
                    let report = haps::security::bind_report(&home, &release, report)?;
                    if attest_scan {
                        publish_scan(
                            &home,
                            &config,
                            haps::security::attestation(&keys, &release, &report)?,
                        )
                        .await?;
                    }
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
            refresh_discovery(&home, &mut config, &mut trust, None, None, Some(&query)).await?;
            let mut results = candidates(&home, &mut config, Some(&query), None).await?;
            let releases: Vec<_> = results.iter().map(|c| c.release.clone()).collect();
            refresh_feedback(&home, &config, &mut trust, &releases).await?;
            results.retain(|c| trust.socially_trusted(&c.release));
            // Known authors sort before publishers vouched for by the graph; fewer hops first.
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
            let choices = candidates(&home, &mut config, None, Some(&package)).await?;
            let releases: Vec<_> = choices.iter().map(|c| c.release.clone()).collect();
            refresh_feedback(&home, &config, &mut trust, &releases).await?;
            let candidate = select(
                choices,
                &package,
                version.as_ref(),
                &config.aliases,
                &trust,
                target(),
                Selection {
                    non_interactive,
                    include_untrusted: false,
                    confirm_install: false,
                },
            )?;
            print_release(&candidate.release, &trust, &config.aliases)?;
        }
        Command::Install {
            package,
            version,
            allow_untrusted,
            allow_warnings,
            require_attestations,
            allow_unaudited,
            audit,
            audit_review,
            json,
        } => {
            progress.stage("Reading package catalogs");
            let choices = candidates(&home, &mut config, None, Some(&package)).await?;
            let releases: Vec<_> = choices.iter().map(|c| c.release.clone()).collect();
            progress.stage("Checking release attestations and warnings");
            refresh_feedback(&home, &config, &mut trust, &releases).await?;
            progress.clear();
            let candidate = select(
                choices,
                &package,
                version.as_ref(),
                &config.aliases,
                &trust,
                target(),
                Selection {
                    non_interactive,
                    include_untrusted: allow_untrusted,
                    confirm_install: true,
                },
            )?;
            let require_attestations = require_attestations.max(1).max(
                installation
                    .receipts()?
                    .get(&candidate.release.identity())
                    .map_or(0, |r| r.minimum_attestations),
            );
            ensure_audited(
                &home,
                &mut config,
                &mut trust,
                &candidate,
                AuditOptions {
                    requested: audit,
                    review: &audit_review,
                    non_interactive,
                    allow_untrusted: allow_untrusted || package.contains('/'),
                    allow_warnings,
                    allow_unaudited,
                    minimum: require_attestations,
                },
            )
            .await?;
            if !trust.socially_trusted(&candidate.release) {
                eprintln!(
                    "Warning: {} is not authored or vouched for by your social graph (or its author/vouchers are overmuted); proceeding with your explicit choice.",
                    release_label(&candidate.release, &config.aliases)
                );
            }
            if !json {
                print_install_start(&candidate.release, &trust, &config.aliases);
            }
            installation
                .install_with_policy(
                    &candidate.repository,
                    &candidate.release,
                    require_attestations,
                )
                .await?;
            progress.clear();
            print_installed(&candidate.release, &trust, &config.aliases, json)?;
        }
        Command::Update {
            package,
            allow_untrusted,
            allow_warnings,
            allow_unaudited,
            audit,
            audit_review,
            json,
        } => {
            let receipt = installation.receipt(&package)?;
            let current = Release::verify(receipt.current)?;
            progress.stage("Reading package catalogs");
            let choices = candidates(&home, &mut config, None, Some(&current.identity())).await?;
            let releases: Vec<_> = choices.iter().map(|c| c.release.clone()).collect();
            progress.stage("Checking release attestations and warnings");
            refresh_feedback(&home, &config, &mut trust, &releases).await?;
            progress.clear();
            let candidate = select(
                choices,
                &current.identity(),
                None,
                &config.aliases,
                &trust,
                target(),
                Selection {
                    non_interactive,
                    include_untrusted: true,
                    confirm_install: false,
                },
            )?;
            ensure_audited(
                &home,
                &mut config,
                &mut trust,
                &candidate,
                AuditOptions {
                    requested: audit,
                    review: &audit_review,
                    non_interactive,
                    allow_untrusted: allow_untrusted || package.contains('/'),
                    allow_warnings,
                    allow_unaudited,
                    minimum: receipt.minimum_attestations.max(1),
                },
            )
            .await?;
            if !trust.socially_trusted(&candidate.release) {
                eprintln!(
                    "Warning: {} is not authored or vouched for by your social graph (or its author/vouchers are overmuted); proceeding with your explicit choice.",
                    release_label(&candidate.release, &config.aliases)
                );
            }
            if !json {
                print_install_start(&candidate.release, &trust, &config.aliases);
            }
            installation
                .install_with_policy(
                    &candidate.repository,
                    &candidate.release,
                    receipt.minimum_attestations.max(1),
                )
                .await?;
            progress.clear();
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
            let mut process = std::process::Command::new(executable);
            #[cfg(target_os = "linux")]
            process.env(
                "XDG_DATA_DIRS",
                haps::launch::linux_data_dirs(
                    &installation.path(&package)?,
                    std::env::var_os("XDG_DATA_DIRS").as_deref(),
                )?,
            );
            drop(guard);
            let status = process.args(args).status()?;
            return Ok(status
                .code()
                .and_then(|c| u8::try_from(c).ok())
                .unwrap_or(1));
        }
        Command::Path { package } => println!("{}", installation.path(&package)?.display()),
        #[cfg(target_os = "linux")]
        Command::Omarchy { remove } => {
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
        Command::Rollback {
            package,
            allow_unaudited,
        } => {
            let receipt = installation.receipt(&package)?;
            let release =
                Release::verify(receipt.previous.context("no previous version retained")?)?;
            refresh_feedback(&home, &config, &mut trust, std::slice::from_ref(&release)).await?;
            if allow_unaudited {
                trust.authorize_with_policy(&release, true, 0, false)?;
                eprintln!("Warning: bypassing the audit requirement for this operation only.");
            } else {
                trust.authorize_install(&release, true, receipt.minimum_attestations, false)?;
            }
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
        Command::Attest(args) | Command::Warn(args) => {
            let ClaimArgs {
                release,
                version,
                target: requested_target,
                note,
                audited,
                provenance,
                revoke,
                out,
                json,
            } = args;
            let keys = own_keys(&home, &config)?;
            ensure!(
                !is_warning || !audited,
                "--audited is only valid for an approval"
            );
            let audit_evidence = provenance.map(haps::audit::Evidence::manual).transpose()?;
            ensure!(
                !note.trim().is_empty(),
                "describe your finding or checked work with --note"
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
                    candidates(&home, &mut config, None, Some(&package)).await?,
                    &package,
                    Some(&version),
                    &config.aliases,
                    &trust,
                    requested_target.as_deref().unwrap_or(target()),
                    Selection {
                        non_interactive,
                        include_untrusted: false,
                        confirm_install: false,
                    },
                )?;
                if !json {
                    eprintln!(
                        "{} {} {} ({})",
                        if revoke {
                            "Withdrawing claim for"
                        } else if is_warning {
                            "Warning about"
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
            let mut event = if is_warning {
                warn(&keys, release_id, !revoke, note)?
            } else {
                attest(&keys, release_id, !revoke, note)?
            };

            let previous = trust
                .events()
                .into_iter()
                .filter(|e| {
                    e.pubkey == keys.public_key()
                        && parse_attestation(e).is_ok_and(|a| a.release == release_id.to_hex())
                })
                .map(|e| attestation_time_ms(&e))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .max();
            if let Some(previous) = previous
                && previous / 1000 >= event.created_at.as_secs()
            {
                // Relays replace addressable events by whole seconds, not our
                // fact snapshot's millisecond field. Rapid edits must advance
                // that timestamp too, or a warning/revocation can be discarded.
                let claim = parse_attestation(&event)?;
                let build = if is_warning { warn_at } else { attest_at };
                event = build(
                    &keys,
                    release_id,
                    if is_warning {
                        claim.warning
                    } else {
                        claim.approved
                    },
                    claim.note,
                    (previous / 1000)
                        .checked_add(1)
                        .and_then(|seconds| seconds.checked_mul(1000))
                        .context("attestation timestamp overflow")?,
                )?;
            }
            if let Some(evidence) = audit_evidence {
                event = haps::trust::attest_audit_at(
                    &keys,
                    release_id,
                    parse_attestation(&event)?.note,
                    &evidence,
                    attestation_time_ms(&event)?,
                )?;
            }
            trust.ingest(event.clone())?;
            save_trust(&home, &trust)?;
            let out =
                out.unwrap_or_else(|| home.join("attestations").join(format!("{}.json", event.id)));
            atomic_write(&out, &serde_json::to_vec_pretty(&event)?)?;
            publish_feedback(&home, &config, &event).await?;
            if json {
                println!("{}", serde_json::to_string(&event)?);
            } else {
                println!("{}", event.id);
                eprintln!("Saved signed claim: {}", out.display());
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
                candidates(&home, &mut config, None, Some(&package)).await?,
                &package,
                release.as_deref(),
            )?;
            refresh_feedback(
                &home,
                &config,
                &mut trust,
                std::slice::from_ref(&candidate.release),
            )
            .await?;
            let mut comments = load_comments(&home)?;
            let parent = reply_to.as_deref().map(EventId::from_hex).transpose()?;
            let parent = parent
                .map(|id| {
                    comments
                        .get(&id)
                        .context("reply target is unavailable; try haps comments first")
                })
                .transpose()?;
            let root = if release.is_some() {
                candidate.release.event.clone()
            } else {
                let cid = candidate
                    .package_card
                    .as_ref()
                    .context("package card is missing")?;
                let event: Event = candidate.repository.json(cid).await?;
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
            if let Some(out) = out {
                atomic_write(&out, &serde_json::to_vec_pretty(&event)?)?;
            }
            publish_feedback(&home, &config, &event).await?;
            println!("{}", event.id);
        }
        Command::Comments { package, release } => {
            let candidate = select_discussion(
                candidates(&home, &mut config, None, Some(&package)).await?,
                &package,
                release.as_deref(),
            )?;
            refresh_feedback(
                &home,
                &config,
                &mut trust,
                std::slice::from_ref(&candidate.release),
            )
            .await?;
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

fn networking_enabled(config: &Config) -> bool {
    config.starting_point.is_some() || std::env::var_os("NOSTR_RELAYS").is_some()
}

async fn publish_scan(home: &Path, config: &Config, event: Event) -> Result<()> {
    let mut comments = load_comments(home)?;
    comments.ingest(event.clone())?;
    save_comments(home, &comments)?;
    publish_feedback(home, config, &event).await?;
    eprintln!(
        "Signed source scan: {} (evidence only; no release endorsement)",
        event.id
    );
    Ok(())
}

async fn publish_feedback(home: &Path, config: &Config, event: &Event) -> Result<()> {
    let discovery = Discovery::open(home)?;
    discovery.queue(std::slice::from_ref(event))?;
    if networking_enabled(config) {
        let result = async {
            let bus = haps::discovery::relay_bus(home).await?;
            discovery.flush(&bus).await
        }
        .await;
        match result {
            Ok(0) => eprintln!("Signed feedback acknowledged by a relay"),
            Ok(_) => eprintln!("Signed feedback queued; retry with haps sync"),
            Err(error) => eprintln!("Signed feedback queued: {error:#}. Retry with haps sync"),
        }
    }
    Ok(())
}

async fn refresh_feedback(
    home: &Path,
    config: &Config,
    trust: &mut Trust,
    releases: &[Release],
) -> Result<()> {
    if releases.is_empty() {
        return Ok(());
    }
    let result = async {
        let relay = if networking_enabled(config) {
            match haps::discovery::relay_bus(home).await {
                Ok(bus) => Some(Arc::new(bus)),
                Err(error) => {
                    eprintln!("Relay feedback unavailable; using indexes: {error:#}");
                    None
                }
            }
        } else {
            None
        };
        let indexes: Vec<_> = config
            .indexes
            .values()
            .map(|source| (source.location.clone(), source.author.clone()))
            .collect();
        let lookup = Discovery::open(home)?
            .event_lookup(relay, &indexes, std::time::Duration::from_secs(3))
            .await?;
        let mut events = Vec::new();
        for batch in releases.chunks(128) {
            events.extend(haps::feedback::refresh(&lookup, batch).await?);
        }
        Ok::<_, anyhow::Error>(events)
    }
    .await;
    let events = match result {
        Ok(events) => events,
        Err(error) => {
            eprintln!("Feedback refresh unavailable; cached findings may be incomplete: {error:#}");
            return Ok(());
        }
    };
    let mut comments = load_comments(home)?;
    for event in events {
        if event.kind == Kind::Comment {
            comments.ingest(event)?;
        } else {
            trust.ingest(event)?;
        }
    }
    save_trust(home, trust)?;
    save_comments(home, &comments)
}

async fn flush_discovery(home: &Path, discovery: &Discovery) {
    match haps::discovery::relay_bus(home).await {
        Ok(bus) => match discovery.flush(&bus).await {
            Ok(0) => println!("Announcements acknowledged by a relay"),
            Ok(pending) => eprintln!("{pending} announcements queued; retry with haps sync"),
            Err(error) => eprintln!("Announcements remain queued: {error:#}"),
        },
        Err(error) => eprintln!("Announcements remain queued: {error:#}. Retry with haps sync"),
    }
}

async fn discover_social_indexes(
    home: &Path,
    config: &mut Config,
    trust: &mut Trust,
    discovery: &Discovery,
    relay: Option<&Arc<nostr_pubsub_relay::RelayEventBus>>,
) -> Result<()> {
    use haps::event_catalog::{INDEX_KIND, IndexAnnouncement};
    let indexes: Vec<_> = config
        .indexes
        .values()
        .map(|source| (source.location.clone(), source.author.clone()))
        .collect();
    let lookup = discovery
        .event_lookup(relay.cloned(), &indexes, std::time::Duration::from_secs(2))
        .await?;
    let mut queried = BTreeSet::new();
    for _ in 0..2 {
        let authors: Vec<_> = trust
            .discovery_authors(1)
            .into_iter()
            .filter(|key| queried.insert(*key))
            .collect();
        if authors.is_empty() {
            break;
        }
        let events = lookup
            .query(vec![
                nostr::Filter::new()
                    .kinds([Kind::ContactList, Kind::MuteList])
                    .authors(authors)
                    .limit(512),
            ])
            .await?;
        for event in events {
            if let Err(error) = trust.ingest(event) {
                eprintln!("Invalid social event: {error:#}");
            }
        }
        save_trust(home, trust)?;
    }
    let authors = trust.discovery_authors(2);
    if !authors.is_empty() {
        let events = lookup
            .query(vec![
                nostr::Filter::new()
                    .kinds([INDEX_KIND, APP_KIND])
                    .custom_tag(
                        nostr::SingleLetterTag::lowercase(nostr::Alphabet::L),
                        "hashtree",
                    )
                    .authors(authors)
                    .limit(256),
            ])
            .await?;
        discovery.ingest(events).await?;
    }
    config.indexes.retain(|name, source| {
        !name.starts_with("social-")
            || (!trust.overmuted(&source.author)
                && trust.distance(&source.author).is_some_and(|d| d <= 2))
    });
    for event in discovery
        .events(vec![nostr::Filter::new().kinds([INDEX_KIND, APP_KIND])])
        .await?
    {
        let Ok(announcement) = IndexAnnouncement::verify(event) else {
            continue;
        };
        let author = announcement.event.pubkey.to_hex();
        if trust.overmuted(&author) || !trust.distance(&author).is_some_and(|d| d <= 2) {
            continue;
        }
        let name = format!(
            "social-{author}-{}",
            hex::encode(tag_value(&announcement.event, "d")?)
        );
        config.indexes.insert(
            name,
            Source {
                location: announcement.location,
                author,
                sequence: 0,
                event_id: String::new(),
            },
        );
    }
    Ok(())
}

async fn refresh_discovery(
    home: &Path,
    config: &mut Config,
    trust: &mut Trust,
    publisher: Option<nostr::PublicKey>,
    package: Option<&str>,
    query: Option<&str>,
) -> Result<()> {
    let discovery = Discovery::open(home)?;
    let relay = if networking_enabled(config) {
        match haps::discovery::relay_bus(home).await {
            Ok(bus) => Some(Arc::new(bus)),
            Err(error) => {
                eprintln!("Relay discovery unavailable; using indexes: {error:#}");
                None
            }
        }
    } else {
        None
    };
    discover_social_indexes(home, config, trust, &discovery, relay.as_ref()).await?;
    let indexes: Vec<_> = config
        .indexes
        .values()
        .map(|source| (source.location.clone(), source.author.clone()))
        .collect();
    discovery
        .lookup_sources(
            relay,
            &indexes,
            publisher,
            package,
            query,
            std::time::Duration::from_secs(3),
        )
        .await?;
    for announcement in discovery.announcements(publisher).await? {
        if announcement.head.is_some() {
            continue;
        }
        if let Some(query) = query {
            let text =
                format!("{} {}", announcement.name, announcement.event.content).to_lowercase();
            if !query
                .split_whitespace()
                .any(|term| text.contains(&term.to_lowercase()))
            {
                continue;
            }
        }
        let author = announcement.author();
        if config
            .sources
            .values()
            .any(|s| s.author == author && s.location == announcement.location)
        {
            continue;
        }
        let name = format!("discovered-{}-{}", author, announcement.name);
        let source = config.sources.entry(name).or_insert_with(|| Source {
            location: announcement.location.clone(),
            author,
            sequence: 0,
            event_id: String::new(),
        });
        // Keep catalog sequence pinning when a publisher changes hosting.
        source.location = announcement.location;
    }
    save_config(home, config)
}
