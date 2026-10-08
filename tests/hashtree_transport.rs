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
            resolve_window: Duration::from_secs(1),
            request_timeout: Duration::from_secs(3),
        };
        let repository = Repository::hashtree(&location, &cache, config)?;
        let snapshot = repository.catalog(&publisher.public_key().to_hex()).await?;
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
    let output = run(&["install", "hello", "--allow-untrusted", "--json"])?;
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(root_requests.load(Ordering::Relaxed), 1);
    assert_eq!(
        fs::read(Installation::new(cli_home)?.command("hello", None)?)?,
        b"verified app payload"
    );
    task.abort();
    Ok(())
}
