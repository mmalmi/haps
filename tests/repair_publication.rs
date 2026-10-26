#![cfg(unix)]
use haps::{
    audit::{BuildIdentity, Evidence},
    build::{Builder, Recipe, Steps},
    model::{PackageSpec, SourceInfo, target},
    repository::Repository,
};
use nostr::{Event, Keys};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, process::Command};

#[path = "support/network.rs"]
mod network;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repair_publication_checks_review_scopes_upload_and_waits_for_acknowledgements()
-> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().join("home");
    let session = home.join("audits/repairs/source-AbC123");
    let catalog = session.join("builds/build-XyZ123/catalog");
    let payload = tmp.path().join("payload");
    fs::create_dir_all(&payload)?;
    fs::write(payload.join("hello"), "repaired bytes")?;
    let keys = Keys::generate();
    fs::create_dir_all(&home)?;
    fs::write(home.join("identity.key"), keys.secret_key().to_secret_hex())?;
    let package = PackageSpec {
        name: "hello".into(),
        version: "1.0.1".parse()?,
        target: target().into(),
        description: "Repaired fixture".into(),
        commands: BTreeMap::from([("hello".into(), "hello".into())]),
        dependencies: vec![],
        source: Some(SourceInfo {
            git: "https://example.invalid/fork".into(),
            rev: "b".repeat(40),
        }),
        app: None,
        desktop: None,
    };
    let repo = Repository::local(catalog.clone())?;
    let release = repo.publish(&keys, package.clone(), &payload).await?;
    let mut original = package.clone();
    original.version = "1.0.0".parse()?;
    original.source = Some(SourceInfo {
        git: "file:///private/original-source".into(),
        rev: "a".repeat(40),
    });
    let recipe = Recipe {
        package: original,
        build: Steps {
            unavailable: None,
            builder: Builder::Commands,
            commands: vec![vec!["unused".into()]],
            artifacts: BTreeMap::from([("hello".into(), "hello".into())]),
        },
    };
    let failed = serde_json::to_vec(&serde_json::json!({
        "schema": "haps.failed-audit.v1", "error": "PRIVATE FINDING AND PATH",
        "recipe": recipe,
    }))?;
    fs::write(session.join("audit-failure.json"), &failed)?;
    fs::write(
        session.join("ready.json"),
        serde_json::to_vec(&serde_json::json!({
            "attempt": "build-XyZ123", "release": release.event.id,
        }))?,
    )?;
    let accept = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let received = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<Event>::new()));
    let app = axum::Router::new()
        .route("/", axum::routing::get(network::relay))
        .route("/catalog/*path", axum::routing::get(network::catalog_file))
        .with_state(network::RelayState {
            events: received.clone(),
            catalog: catalog.clone(),
            accept: accept.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let relay = format!("ws://{}/", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin)?;
    let helper = bin.join("htree");
    fs::write(
        &helper,
        r#"#!/usr/bin/env python3
import os, pathlib, sys
if sys.argv[1:] == ['--version']:
    print('htree 0.2.151')
    sys.exit(0)
assert sys.argv[1] == 'add'
assert sys.argv[3:] == ['--publish', 'fixed-hello']
catalog = pathlib.Path(sys.argv[2])
assert catalog.resolve() == pathlib.Path(os.environ['EXPECTED_CATALOG']).resolve()
assert not (catalog / 'audit-failure.json').exists()
assert not (catalog / 'ready.json').exists()
assert not (catalog / 'repo').exists()
pathlib.Path(os.environ['UPLOAD_MARKER']).write_text(str(catalog))
print('url: ' + os.environ['UPLOAD_ROOT'])
"#,
    )?;
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755))?;
    let marker = tmp.path().join("uploaded");
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))?;
    let run = || -> anyhow::Result<std::process::Output> {
        Ok(Command::new(env!("CARGO_BIN_EXE_haps"))
            .args(["--no-defaults", "--non-interactive", "--home"])
            .arg(&home)
            .args([
                "audit",
                "repair",
                "publish",
                "source-AbC123",
                "--name",
                "fixed-hello",
            ])
            .env("PATH", &path)
            .env("NOSTR_RELAYS", &relay)
            .env("HTREE_CONFIG_DIR", tmp.path().join("htree"))
            .env("HTREE_PREFER_LOCAL_DAEMON", "false")
            .env("EXPECTED_CATALOG", &catalog)
            .env("UPLOAD_MARKER", &marker)
            .env("UPLOAD_ROOT", hashtree_core::nhash_encode(&[9; 32])?)
            .output()?)
    };
    let identity = Command::new(env!("CARGO_BIN_EXE_haps"))
        .args(["--no-defaults", "--home"])
        .arg(&home)
        .args(["identity", "use", &keys.public_key().to_hex()])
        .env("HTREE_CONFIG_DIR", tmp.path().join("htree"))
        .output()?;
    assert!(
        identity.status.success(),
        "{}",
        String::from_utf8_lossy(&identity.stderr)
    );
    let denied = run()?;
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("passing local audit"),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );
    assert!(!marker.exists());
    let audit = haps::trust::attest_audit_at(
        &keys,
        release.event.id,
        "Reviewed the fixed fixture".into(),
        &Evidence {
            schema: "haps.audit.v1".into(),
            method: "manual-review-local-build".into(),
            provenance: "Reviewed fixture; repair of original source commit".into(),
            binary_match: false,
            reviewer: None,
            build: Some(BuildIdentity {
                recipe_sha256: "c".repeat(64),
                manifest: release.data.manifest.clone(),
            }),
        },
        nostr::Timestamp::now().as_secs() * 1000,
    )?;
    fs::write(home.join("social.json"), serde_json::to_vec(&[&audit])?)?;
    let rejected = run()?;
    assert!(
        !rejected.status.success(),
        "rejected acknowledgements cannot report success"
    );
    assert!(
        marker.exists(),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    let queued = fs::read_dir(home.join("discovery/outbox"))?.count();
    assert_eq!(
        queued, 3,
        "only repaired release, package head, and its audit are shared"
    );
    accept.store(true, std::sync::atomic::Ordering::Relaxed);
    let published = run()?;
    assert!(
        published.status.success(),
        "{}",
        String::from_utf8_lossy(&published.stderr)
    );
    assert_eq!(fs::read_dir(home.join("discovery/outbox"))?.count(), 0);
    let events = received.lock().await.clone();
    assert_eq!(events.len(), 3);
    assert!(events.iter().any(|e| e.id == audit.id));
    assert!(events.iter().any(|e| e.id == release.event.id));
    let public = serde_json::to_string(&events)?;
    assert!(!public.contains("PRIVATE FINDING") && !public.contains("file:///private/"));
    let reader_keys = Keys::generate();
    let mut reader = haps::trust::Trust::new(reader_keys.public_key().to_hex());
    reader.ingest(
        nostr::EventBuilder::new(nostr::Kind::ContactList, "")
            .tags([nostr::Tag::public_key(keys.public_key())])
            .sign_with_keys(&reader_keys)?,
    )?;
    reader.ingest(audit.clone())?;
    assert!(
        reader.audits(&release).is_empty(),
        "publisher self-review must not approve for others"
    );
    fs::remove_file(&marker)?;
    let revoke = haps::trust::attest_at(
        &keys,
        release.event.id,
        false,
        "Withdrawing repair review".into(),
        (nostr::Timestamp::now().as_secs() + 5) * 1000,
    )?;
    fs::write(
        home.join("social.json"),
        serde_json::to_vec(&[audit, revoke])?,
    )?;
    assert!(!run()?.status.success());
    assert!(
        !marker.exists(),
        "revocation must be checked before uploading"
    );
    let mut unrelated = package;
    unrelated.name = "unrelated".into();
    repo.publish(&keys, unrelated, &payload).await?;
    let denied = run()?;
    assert!(String::from_utf8_lossy(&denied.stderr).contains("exactly one reviewed release"));
    assert!(!marker.exists());
    assert_eq!(fs::read(session.join("audit-failure.json"))?, failed);
    server.abort();
    Ok(())
}
