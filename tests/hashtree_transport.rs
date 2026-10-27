#[path = "support/audit.rs"]
mod audit;
use axum::{
    Router,
    extract::{Path, State, WebSocketUpgrade, ws::Message},
    response::IntoResponse,
    routing::get,
};
use haps::{
    install::Installation,
    model::{PackageSpec, target},
    repository::Repository,
};
use hashtree_client::ClientConfig;
use hashtree_core::{HashTree, HashTreeConfig, MemoryStore, Store};
use nostr::{Event, EventBuilder, Keys, Kind, Tag, nips::nip19::ToBech32};
use std::{
    collections::BTreeMap,
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

#[path = "support/tree.rs"]
mod tree;
use tree::add_directory;

#[derive(Clone)]
struct Fixture {
    store: Arc<MemoryStore>,
    event: Event,
    delay_ms: Arc<AtomicU64>,
    one_root_only: Arc<AtomicBool>,
    root_requests: Arc<AtomicU64>,
}

async fn ws(State(state): State<Fixture>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade.on_upgrade(move |mut socket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            let Message::Text(text) = message else {
                continue;
            };
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            if value[0] == "REQ" {
                let id = &value[1];
                let root_request = value[2]["#d"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag == "packages"));
                let answer = root_request
                    && (!state.one_root_only.load(Ordering::Relaxed)
                        || state.root_requests.fetch_add(1, Ordering::Relaxed) == 0);
                if answer {
                    tokio::time::sleep(Duration::from_millis(
                        state.delay_ms.load(Ordering::Relaxed),
                    ))
                    .await;
                }
                if answer
                    && socket
                        .send(Message::Text(
                            serde_json::json!(["EVENT", id, state.event]).to_string(),
                        ))
                        .await
                        .is_err()
                {
                    return;
                }
                if socket
                    .send(Message::Text(serde_json::json!(["EOSE", id]).to_string()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    })
}

async fn blob(State(state): State<Fixture>, Path(hash): Path<String>) -> impl IntoResponse {
    let hash: [u8; 32] = hex::decode(
        hash.strip_suffix(".bin")
            .expect("raw block requests require .bin"),
    )
    .unwrap()
    .try_into()
    .unwrap();
    match state.store.get(&hash).await.unwrap() {
        Some(bytes) => bytes.into_response(),
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signed_catalog_search_and_install_through_daemon_and_standalone() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let publisher = Keys::generate();
    let host = Keys::generate();
    let payload = temp.path().join("payload");
    fs::create_dir_all(payload.join("bin"))?;
    fs::write(payload.join("bin/hello"), b"verified app payload")?;
    let repo_dir = temp.path().join("catalog");
    let local = Repository::local(repo_dir.clone())?;
    let release = local
        .publish(
            &publisher,
            PackageSpec {
                name: "hello".into(),
                version: "1.0.0".parse()?,
                target: target().into(),
                description: "Friendly transport fixture".into(),
                commands: BTreeMap::from([("hello".into(), "bin/hello".into())]),
                dependencies: Vec::new(),
                source: None,
                app: None,
                desktop: None,
            },
            &payload,
        )
        .await?;
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store.clone()));
    let root = add_directory(&tree, &repo_dir).await?;
    let mut tags = vec![
        Tag::identifier("packages"),
        Tag::parse(["hash", &hex::encode(root.hash)])?,
        Tag::parse(["l", "hashtree"])?,
    ];
    if let Some(key) = root.key {
        tags.push(Tag::parse(["key", &hex::encode(key)])?);
    }
    let event = EventBuilder::new(Kind::Custom(30064), "")
        .tags(tags)
        .sign_with_keys(&host)?;
    let delay_ms = Arc::new(AtomicU64::new(0));
    let one_root_only = Arc::new(AtomicBool::new(false));
    let root_requests = Arc::new(AtomicU64::new(0));
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(ws))
        .route("/:hash", get(blob))
        .with_state(Fixture {
            store,
            event,
            delay_ms: delay_ms.clone(),
            one_root_only: one_root_only.clone(),
            root_requests: root_requests.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let location = format!("htree://{}/packages", host.public_key().to_bech32()?);
    for daemon in [false, true] {
        let cache = temp.path().join(format!("cache-{daemon}"));
        let config = ClientConfig {
            daemon_url: daemon.then(|| url.clone()),
            local_only: daemon,
            relays: if daemon {
                vec![]
            } else {
                vec![url.replace("http:", "ws:") + "/ws"]
            },
            read_servers: if daemon { vec![] } else { vec![url.clone()] },
            resolve_window: Duration::from_secs(5),
            request_timeout: Duration::from_secs(3),
        };
        let repository = Repository::hashtree(&location, &cache, config)?;
        let snapshot = tokio::time::timeout(
            Duration::from_secs(2),
            repository.catalog(&publisher.public_key().to_hex()),
        )
        .await
        .expect("an available signed root must not wait for the observation window")?;
        assert!(
            repository
                .catalog(&host.public_key().to_hex())
                .await
                .is_err(),
            "hosting identity must not override the pinned package signer"
        );
        let results = repository.search(&snapshot, "friendly").await?;
        assert_eq!(results[0].event.id, release.event.id);
        let installed = Installation::new(temp.path().join(format!("home-{daemon}")))?;
        installed.install(&repository, &results[0]).await?;
        assert_eq!(
            fs::read(installed.command("hello", None)?)?,
            b"verified app payload"
        );
    }
    // Public relays can take longer than three seconds to connect and respond.
    // Exercise the CLI's actual defaults, with separate profile and transport state.
    delay_ms.store(3500, Ordering::Relaxed);
    let config_dir = temp.path().join("htree-config");
    fs::create_dir(&config_dir)?;
    fs::write(
        config_dir.join("config.toml"),
        format!("[blossom]\nread_servers=[{url:?}]\nservers=[]\n"),
    )?;
    let cli_home = temp.path().join("slow-relay-reader");
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_haps"))
            .env("HAPS_HOME", &cli_home)
            .env("HAPS_NO_DEFAULTS", "true")
            .env("HTREE_CONFIG_DIR", &config_dir)
            .env("HTREE_PREFER_LOCAL_DAEMON", "false")
            .env("NOSTR_RELAYS", url.replace("http:", "ws:") + "/ws")
            .args(args)
            .output()
    };
    let output = run(&[
        "source",
        "add",
        "fixture",
        &location,
        "--author",
        &publisher.public_key().to_hex(),
    ])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(run(&["identity", "init"])?.status.success());
    audit::local(&cli_home, release.event.id)?;
    // Once discovery authenticates the root, installation must keep using it.
    // The relay stops answering subsequent requests, while content stays available.
    delay_ms.store(0, Ordering::Relaxed);
    one_root_only.store(true, Ordering::Relaxed);
    let output = run(&[
        "install",
        &release.identity(),
        "--allow-untrusted",
        "--json",
    ])?;
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(root_requests.load(Ordering::Relaxed), 1);
    assert_eq!(
        fs::read(Installation::new(cli_home.clone())?.command("hello", None)?)?,
        b"verified app payload"
    );
    // Immutable installs need neither named-root discovery nor a configured source.
    let hash = hashtree_core::nhash_encode_full(&hashtree_core::NHashData {
        hash: root.hash,
        decrypt_key: root.key,
    })?;
    let named_requests = root_requests.load(Ordering::Relaxed);
    for reference in [hash.clone(), format!("htree://{hash}")] {
        let home = temp.path().join(if reference.starts_with("htree:") {
            "hash-url"
        } else {
            "bare-hash"
        });
        let run_hash = |args: &[&str]| {
            std::process::Command::new(env!("CARGO_BIN_EXE_haps"))
                .env("HAPS_HOME", &home)
                .env("HAPS_NO_DEFAULTS", "true")
                .env("HAPS_NON_INTERACTIVE", "true")
                .env("HTREE_CONFIG_DIR", &config_dir)
                .env("HTREE_PREFER_LOCAL_DAEMON", "false")
                .env_remove("NOSTR_RELAYS")
                .env("XDG_DATA_HOME", home.join("data"))
                .args(args)
                .output()
        };
        assert!(run_hash(&["identity", "init"])?.status.success());
        let denied = run_hash(&["install", &reference, "--json"])?;
        assert!(!denied.status.success(), "a hash is not an audit");
        assert!(!home.join("installed.json").exists());
        audit::local(&home, release.event.id)?;
        let installed = run_hash(&["install", &reference, "--json"])?;
        assert!(
            installed.status.success(),
            "{} {}",
            String::from_utf8_lossy(&installed.stdout),
            String::from_utf8_lossy(&installed.stderr)
        );
        assert_eq!(
            fs::read(Installation::new(home.clone())?.command("hello", None)?)?,
            b"verified app payload"
        );
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join("config.json"))?)?;
        assert!(config["sources"].as_object().unwrap().is_empty());
        assert!(
            run_hash(&[
                "warn",
                &release.event.id.to_hex(),
                "--note",
                "Blocking fixture finding"
            ])?
            .status
            .success()
        );
        assert!(
            !run_hash(&["install", &reference, "--json"])?
                .status
                .success(),
            "hash installs must honor release warnings"
        );
    }
    assert_eq!(root_requests.load(Ordering::Relaxed), named_requests);

    let mut other = release.data.package.clone();
    other.name = "second".into();
    other.commands = BTreeMap::from([("helper".into(), "bin/hello".into())]);
    let helper = local.publish(&publisher, other, &payload).await?;
    let multiple = add_directory(&tree, &repo_dir).await?;
    let multiple = hashtree_core::nhash_encode_full(&hashtree_core::NHashData {
        hash: multiple.hash,
        decrypt_key: multiple.key,
    })?;
    let ambiguity = run(&["install", &multiple, "--json"])?;
    assert!(!ambiguity.status.success());
    assert!(
        format!(
            "{} {}",
            String::from_utf8_lossy(&ambiguity.stdout),
            String::from_utf8_lossy(&ambiguity.stderr)
        )
        .contains("--select publisher/name")
    );
    assert!(
        run(&["install", &multiple, "--select", "hello", "--json"])?
            .status
            .success()
    );
    // Resolve exact dependency pins from the same hash, without registering a
    // named source or accepting a dependency merely because the parent is audited.
    let mut dependent = release.data.package.clone();
    dependent.version = "1.1.0".parse()?;
    dependent.dependencies = vec![haps::dependencies::Dependency {
        package: helper.identity(),
        release: helper.event.id.to_hex(),
    }];
    let dependent = local.publish(&publisher, dependent, &payload).await?;
    let closure = add_directory(&tree, &repo_dir).await?;
    let closure = hashtree_core::nhash_encode_full(&hashtree_core::NHashData {
        hash: closure.hash,
        decrypt_key: closure.key,
    })?;
    let dependency_home = temp.path().join("hash-dependencies");
    let run_dependency = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_haps"))
            .env("HAPS_HOME", &dependency_home)
            .env("HAPS_NO_DEFAULTS", "true")
            .env("HAPS_NON_INTERACTIVE", "true")
            .env_remove("NOSTR_RELAYS")
            .env("HTREE_CONFIG_DIR", &config_dir)
            .env("HTREE_PREFER_LOCAL_DAEMON", "false")
            .env("XDG_DATA_HOME", dependency_home.join("data"))
            .args(args)
            .output()
    };
    assert!(run_dependency(&["identity", "init"])?.status.success());
    audit::local(&dependency_home, dependent.event.id)?;
    let install_args = ["install", &closure, "--select", "hello", "--json"];
    assert!(
        !run_dependency(&install_args)?.status.success(),
        "dependency still needs its own audit"
    );
    assert!(!dependency_home.join("installed.json").exists());
    audit::local(&dependency_home, helper.event.id)?;
    let installed = run_dependency(&install_args)?;
    assert!(
        installed.status.success(),
        "{} {}",
        String::from_utf8_lossy(&installed.stdout),
        String::from_utf8_lossy(&installed.stderr)
    );
    assert_eq!(Installation::new(dependency_home)?.receipts()?.len(), 2);
    // Hash verification does not replace signature verification: these bytes
    // have a valid content address, but a forged catalog signature.
    let catalog_path = repo_dir.join("catalog.json");
    let mut forged: Event = serde_json::from_slice(&fs::read(&catalog_path)?)?;
    forged.content.push(' ');
    fs::write(&catalog_path, serde_json::to_vec(&forged)?)?;
    let forged = add_directory(&tree, &repo_dir).await?;
    let forged = hashtree_core::nhash_encode_full(&hashtree_core::NHashData {
        hash: forged.hash,
        decrypt_key: forged.key,
    })?;
    assert!(
        !run(&["install", &forged, "--allow-unaudited", "--json"])?
            .status
            .success()
    );
    assert!(
        !run(&["install", "nhash1invalid", "--allow-unaudited", "--json"])?
            .status
            .success()
    );
    task.abort();
    Ok(())
}
