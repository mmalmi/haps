use haps::{
    discovery::Announcement,
    install::Installation,
    model::{PackageSpec, target},
    repository::Repository,
};
use nostr::{Keys, nips::nip19::ToBech32};
use std::{collections::BTreeMap, fs, process::Command, sync::Arc};

#[path = "support/network.rs"]
mod network;
#[path = "support/tree.rs"]
mod tree;

fn package() -> PackageSpec {
    PackageSpec {
        name: "hello".into(),
        version: "1.0.0".parse().unwrap(),
        target: target().into(),
        description: "Friendly direct package".into(),
        commands: BTreeMap::from([("hello".into(), "hello".into())]),
        source: None,
        app: None,
        desktop: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn package_lookup_from_relay_or_index_installs_without_catalog_head() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let publisher = Keys::generate();
    let operator = Keys::generate();
    let payload = temp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("hello"), b"verified direct payload")?;
    let catalog = temp.path().join("packages");
    let repo = Repository::local(catalog.clone())?;
    let release = repo.publish(&publisher, package(), &payload).await?;
    let snapshot = repo.catalog(&publisher.public_key().to_hex()).await?;
    // Publish only the immutable blocks: there is no mutable catalog head, and
    // the relay has no Hashtree root event for the publisher.
    fs::remove_file(catalog.join("catalog.json"))?;
    let blobs = Arc::new(hashtree_core::MemoryStore::new());
    let tree = hashtree_core::HashTree::new(hashtree_core::HashTreeConfig::new(blobs.clone()));
    let root = tree::add_directory(&tree, &catalog).await?;
    let immutable = format!(
        "htree://{}",
        hashtree_core::nhash_encode_full(&hashtree_core::NHashData {
            hash: root.hash,
            decrypt_key: root.key
        })?
    );
    let state = network::RelayState {
        events: Arc::new(tokio::sync::Mutex::new(vec![])),
        catalog: catalog.clone(),
        accept: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let config_dir = temp.path().join("htree");
    fs::create_dir(&config_dir)?;
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[blossom]\nread_servers=[{:?}]\nservers=[]\n",
            url.clone() + "/blocks"
        ),
    )?;
    let served_blobs = blobs.clone();
    let app = axum::Router::new()
        .route(
            "/blocks/:hash",
            axum::routing::get(
                move |axum::extract::Path(hash): axum::extract::Path<String>| {
                    let blobs = served_blobs.clone();
                    async move {
                        use axum::response::IntoResponse;
                        use hashtree_core::Store;
                        let hash = match hashtree_core::from_hex(hash.trim_end_matches(".bin")) {
                            Ok(hash) => hash,
                            Err(_) => return axum::http::StatusCode::BAD_REQUEST.into_response(),
                        };
                        match blobs.get(&hash).await.unwrap() {
                            Some(bytes) => bytes.into_response(),
                            None => axum::http::StatusCode::NOT_FOUND.into_response(),
                        }
                    }
                },
            ),
        )
        .route("/ws", axum::routing::get(network::relay))
        .route("/catalog/*path", axum::routing::get(network::catalog_file))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let head = Announcement::sign_direct(&publisher, &repo, &snapshot, "hello", &immutable).await?;
    state
        .events
        .lock()
        .await
        .extend([head.clone(), release.event.clone()]);
    // An ordinary mixed Nostr index, built independently of Haps. No index.json.
    let native = hashtree_nostr::NostrEventStore::new(blobs.clone());
    let note = nostr::EventBuilder::text_note("An unrelated event").sign_with_keys(&operator)?;
    let root = native
        .build(
            None,
            [head.clone(), release.event.clone(), note]
                .iter()
                .map(hashtree_nostr::stored_event_from_nostr_sdk_event),
        )
        .await?
        .unwrap();
    let index_url = haps::event_catalog::root_url(&root)?;
    for mode in ["index", "relay", "both"] {
        let home = temp.path().join(mode);
        let use_relay = mode != "index";
        let run = |args: &[&str]| {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_haps"));
            cmd.env("HAPS_HOME", &home)
                .env("HAPS_NO_DEFAULTS", "true")
                .env("HTREE_CONFIG_DIR", temp.path().join("htree"))
                .env("HTREE_PREFER_LOCAL_DAEMON", "false")
                .env("XDG_DATA_HOME", home.join("data"));
            if use_relay {
                cmd.env("NOSTR_RELAYS", url.replace("http:", "ws:") + "/ws");
            } else {
                cmd.env_remove("NOSTR_RELAYS");
            }
            cmd.args(args).output().unwrap()
        };
        if mode != "relay" {
            let result = run(&[
                "index",
                "add",
                "friend",
                &index_url,
                "--author",
                &operator.public_key().to_hex(),
            ]);
            anyhow::ensure!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let package = format!("{}/hello", publisher.public_key().to_bech32()?);
        let result = run(&["--non-interactive", "install", &package, "--json"]);
        anyhow::ensure!(
            result.status.success(),
            "{} {}",
            String::from_utf8_lossy(&result.stderr),
            String::from_utf8_lossy(&result.stdout)
        );
        let installed = Installation::new(home)?.with_desktop_dir(None);
        assert_eq!(installed.receipt("hello")?.current, release.event);
        assert_eq!(
            fs::read(installed.command("hello", None)?)?,
            b"verified direct payload"
        );
    }
    // The relay advertises only a friend's index, not the package itself.
    // The user's signed follow list supplies social discovery without manual registration.
    let reader = Keys::generate();
    let follow = nostr::EventBuilder::new(nostr::Kind::ContactList, "")
        .tags([nostr::Tag::public_key(operator.public_key())])
        .sign_with_keys(&reader)?;
    // Match htree's existing root event exactly: no Haps tags or content.
    let mut tags = vec![
        nostr::Tag::identifier("nostr-event-index"),
        nostr::Tag::parse(["l", "hashtree"])?,
        nostr::Tag::parse(["hash", &hex::encode(root.hash)])?,
    ];
    if let Some(key) = root.key {
        tags.push(nostr::Tag::parse(["key", &hex::encode(key)])?);
    }
    let advertised = nostr::EventBuilder::new(haps::event_catalog::INDEX_KIND, "")
        .tags(tags)
        .sign_with_keys(&operator)?;
    *state.events.lock().await = vec![follow.clone(), advertised.clone()];
    let home = temp.path().join("social-reader");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_haps"))
            .env("HAPS_HOME", &home)
            .env("HAPS_NO_DEFAULTS", "true")
            .env("HTREE_CONFIG_DIR", temp.path().join("htree"))
            .env("HTREE_PREFER_LOCAL_DAEMON", "false")
            .env("NOSTR_RELAYS", url.replace("http:", "ws:") + "/ws")
            .env("XDG_DATA_HOME", home.join("data"))
            .args(args)
            .output()
            .unwrap()
    };
    assert!(
        run(&["identity", "use", &reader.public_key().to_hex()])
            .status
            .success()
    );
    let search = run(&["search", "friendly", "--json"]);
    anyhow::ensure!(
        search.status.success(),
        "{} {}",
        String::from_utf8_lossy(&search.stderr),
        String::from_utf8_lossy(&search.stdout)
    );
    let results: serde_json::Value = serde_json::from_slice(&search.stdout)?;
    assert!(results.as_array().unwrap().is_empty());
    let denied = run(&["--non-interactive", "install", "hello", "--json"]);
    assert!(
        !denied.status.success(),
        "following the curator must not grant publisher trust"
    );
    assert!(String::from_utf8_lossy(&denied.stdout).contains("outside your social graph"));
    let endorsement = haps::trust::attest(&operator, release.event.id, true, "Checked".into())?;
    state.events.lock().await.push(endorsement.clone());
    let approved = run(&["install", "hello", "--json"]);
    anyhow::ensure!(
        approved.status.success(),
        "{} {}",
        String::from_utf8_lossy(&approved.stderr),
        String::from_utf8_lossy(&approved.stdout)
    );
    let config: serde_json::Value = serde_json::from_slice(&fs::read(home.join("config.json"))?)?;
    assert_eq!(config["indexes"].as_object().unwrap().len(), 1);
    // Root discovery uses the same router: the root advertisement and follow
    // can instead come from another ordinary index, with no relay available.
    let bootstrap = native
        .build(
            None,
            [follow, advertised, endorsement]
                .iter()
                .map(hashtree_nostr::stored_event_from_nostr_sdk_event),
        )
        .await?
        .unwrap();
    let offline = temp.path().join("index-bootstrap");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_haps"))
            .env("HAPS_HOME", &offline)
            .env("HAPS_NO_DEFAULTS", "true")
            .env("HTREE_CONFIG_DIR", &config_dir)
            .env("HTREE_PREFER_LOCAL_DAEMON", "false")
            .env_remove("NOSTR_RELAYS")
            .args(args)
            .output()
            .unwrap()
    };
    assert!(
        run(&["identity", "use", &reader.public_key().to_hex()])
            .status
            .success()
    );
    let added = run(&[
        "catalog",
        "add",
        "bootstrap",
        &haps::event_catalog::root_url(&bootstrap)?,
        "--author",
        &operator.public_key().to_hex(),
    ]);
    anyhow::ensure!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let search = run(&["search", "friendly", "--json"]);
    anyhow::ensure!(
        search.status.success(),
        "{} {}",
        String::from_utf8_lossy(&search.stderr),
        String::from_utf8_lossy(&search.stdout)
    );
    let results: serde_json::Value = serde_json::from_slice(&search.stdout)?;
    assert_eq!(results.as_array().unwrap().len(), 1);
    assert_eq!(results[0]["attesters"][0], operator.public_key().to_hex());
    let installed = run(&["install", "hello", "--json"]);
    anyhow::ensure!(
        installed.status.success(),
        "{} {}",
        String::from_utf8_lossy(&installed.stderr),
        String::from_utf8_lossy(&installed.stdout)
    );
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(offline.join("config.json"))?)?;
    assert_eq!(config["indexes"].as_object().unwrap().len(), 2);
    server.abort();
    Ok(())
}
