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
use hashtree_core::{Cid, DirEntry, HashTree, HashTreeConfig, LinkType, MemoryStore, Store};
use nostr::{Event, EventBuilder, Keys, Kind, Tag, nips::nip19::ToBech32};
use std::{collections::BTreeMap, fs, future::Future, pin::Pin, sync::Arc, time::Duration};

fn add_directory<'a>(
    tree: &'a HashTree<MemoryStore>,
    path: &'a std::path::Path,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Cid>> + Send + 'a>> {
    Box::pin(async move {
        let mut entries = vec![];
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let (cid, size, kind) = if entry.file_type()?.is_dir() {
                (add_directory(tree, &entry.path()).await?, 0, LinkType::Dir)
            } else {
                let (cid, size) = tree.put(&fs::read(entry.path())?).await?;
                (cid, size, LinkType::File)
            };
            entries.push(DirEntry {
                name: entry.file_name().to_str().unwrap().into(),
                hash: cid.hash,
                key: cid.key,
                size,
                link_type: kind,
                meta: None,
            });
        }
        Ok(tree.put_directory(entries).await?)
    })
}

#[derive(Clone)]
struct Fixture {
    store: Arc<MemoryStore>,
    event: Event,
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
                if socket
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

#[tokio::test]
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
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(ws))
        .route("/:hash", get(blob))
        .with_state(Fixture { store, event });
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
    task.abort();
    Ok(())
}
