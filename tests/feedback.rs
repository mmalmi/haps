//! Three independent CLI homes exchange signed feedback through a real websocket.
#[path = "support/network.rs"]
mod network;
use haps::{
    discovery::{Announcement, Discovery},
    model::Release,
};
use nostr::Keys;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

fn cli(root: &Path, user: &str, relay: &str, args: &[&str]) -> Output {
    Command::new(
        std::env::var_os("HAPS_TEST_BIN").unwrap_or_else(|| env!("CARGO_BIN_EXE_haps").into()),
    )
    .env("HAPS_HOME", root.join(user))
    .env("HAPS_NO_DEFAULTS", "true")
    .env("HAPS_NON_INTERACTIVE", "true")
    .env("NOSTR_RELAYS", relay)
    .env("HTREE_CONFIG_DIR", root.join(format!("{user}-htree")))
    .env("HTREE_PREFER_LOCAL_DAEMON", "false")
    .env("XDG_DATA_HOME", root.join(format!("{user}-data")))
    .args(args)
    .output()
    .unwrap()
}
fn ok(root: &Path, user: &str, relay: &str, args: &[&str]) -> String {
    let out = cli(root, user, relay, args);
    assert!(
        out.status.success(),
        "{args:?}: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_users_discover_comments_endorsements_warnings_and_revocations() -> anyhow::Result<()>
{
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let catalog = root.join("catalog");
    let events = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let state = network::RelayState {
        events: events.clone(),
        catalog: catalog.clone(),
        accept: Arc::new(AtomicBool::new(true)),
    };
    let app = axum::Router::new()
        .route("/ws", axum::routing::get(network::relay))
        .route("/catalog/*path", axum::routing::get(network::catalog_file))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let relay = url.replace("http:", "ws:") + "/ws";
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let publisher = ok(root, "publisher", &relay, &["identity", "init"])
        .trim()
        .to_owned();
    let reviewer = ok(root, "reviewer", &relay, &["identity", "init"])
        .trim()
        .to_owned();
    let reader = ok(root, "reader", &relay, &["identity", "init"])
        .trim()
        .to_owned();
    assert_ne!(publisher, reviewer);
    assert_ne!(reader, reviewer);
    assert_ne!(publisher, reader);
    let payload = root.join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("hello"), b"signed payload")?;
    let manifest = root.join("haps.toml");
    fs::write(
        &manifest,
        format!(
            "name='hello'\nversion='1.0.0'\ntarget='{}'\ndescription='Three-user test'\n[commands]\nhello='hello'\n",
            haps::model::target()
        ),
    )?;
    let event: nostr::Event = serde_json::from_str(&ok(
        root,
        "publisher",
        &relay,
        &[
            "pack",
            manifest.to_str().unwrap(),
            "--payload",
            payload.to_str().unwrap(),
            "--out",
            catalog.to_str().unwrap(),
        ],
    ))?;
    let release = Release::verify(event.clone())?;
    let keys = Keys::parse(fs::read_to_string(root.join("publisher/identity.key"))?.trim())?;
    let bus = nostr_pubsub_relay::RelayEventBus::new([&relay], Duration::from_secs(1)).await?;
    let discovery = Discovery::open(&root.join("publisher"))?;
    discovery.queue(&[
        event,
        Announcement::sign(&keys, &release.data.package, &format!("{url}/catalog"))?,
    ])?;
    assert_eq!(discovery.flush(&bus).await?, 0);
    drop(discovery);
    let package = format!("{publisher}/hello");
    let id = release.event.id.to_hex();
    // No user adds a source or copies another user's event file.
    let comment_file = root.join("reviewer/comment.json");
    let comment = ok(
        root,
        "reviewer",
        &relay,
        &[
            "comment",
            &package,
            "I checked the installer",
            "--out",
            comment_file.to_str().unwrap(),
        ],
    )
    .trim()
    .to_owned();
    let listed = ok(root, "reader", &relay, &["comments", &package]);
    assert!(
        listed.contains("I checked the installer"),
        "remote comment missing: {listed}"
    );
    assert!(listed.contains(&reviewer));
    ok(
        root,
        "reviewer",
        &relay,
        &[
            "attest",
            &id,
            "--audited",
            "--provenance",
            "Test fixture",
            "--note",
            "Installer verified",
            "--json",
        ],
    );
    let before: serde_json::Value =
        serde_json::from_str(&ok(root, "reader", &relay, &["info", &package, "--json"]))?;
    assert!(
        before["attestations"].as_array().unwrap().is_empty(),
        "strangers must not count as trusted findings"
    );
    ok(root, "reader", &relay, &["follow", &reviewer]);
    let endorsed: serde_json::Value =
        serde_json::from_str(&ok(root, "reader", &relay, &["info", &package, "--json"]))?;
    assert_eq!(endorsed["attestations"][0]["signer"], reviewer);
    ok(
        root,
        "reader",
        &relay,
        &["install", &package, "--require-attestations", "1", "--json"],
    );
    // Replies are retrieved from the package thread; no manual import is needed.
    ok(
        root,
        "reader",
        &relay,
        &[
            "comment",
            &package,
            "Thanks for checking",
            "--reply-to",
            &comment,
        ],
    );
    assert!(ok(root, "publisher", &relay, &["comments", &package]).contains("Thanks for checking"));
    ok(
        root,
        "publisher",
        &relay,
        &["warn", &id, "--note", "Unfollowed author warning", "--json"],
    );
    let irrelevant: serde_json::Value =
        serde_json::from_str(&ok(root, "reader", &relay, &["info", &package, "--json"]))?;
    assert!(irrelevant["warnings"].as_array().unwrap().is_empty());
    ok(
        root,
        "reviewer",
        &relay,
        &["warn", &id, "--note", "Found a problem", "--json"],
    );
    let warned: serde_json::Value =
        serde_json::from_str(&ok(root, "reader", &relay, &["info", &package, "--json"]))?;
    assert_eq!(warned["warnings"][0]["signer"], reviewer);
    assert!(warned["attestations"].as_array().unwrap().is_empty());
    let denied = cli(
        root,
        "reader",
        &relay,
        &["install", &package, "--allow-untrusted", "--json"],
    );
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stdout).contains("trusted release warnings"));
    ok(
        root,
        "reviewer",
        &relay,
        &[
            "warn",
            &id,
            "--revoke",
            "--note",
            "Finding withdrawn",
            "--json",
        ],
    );
    let revoked: serde_json::Value =
        serde_json::from_str(&ok(root, "reader", &relay, &["info", &package, "--json"]))?;
    assert!(revoked["warnings"].as_array().unwrap().is_empty());
    assert!(revoked["attestations"].as_array().unwrap().is_empty());
    for user in ["publisher", "reviewer", "reader"] {
        assert!(
            fs::read_dir(root.join(user).join("discovery/outbox"))?
                .next()
                .is_none()
        );
    }
    server.abort();
    Ok(())
}
