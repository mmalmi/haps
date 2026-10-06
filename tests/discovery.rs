use haps::{
    discovery::{Announcement, Discovery},
    model::{PackageSpec, target},
};
use nostr::{EventBuilder, Keys, Tag};
use std::collections::BTreeMap;

fn package() -> PackageSpec {
    PackageSpec {
        name: "hello".into(),
        version: "1.0.0".parse().unwrap(),
        target: target().into(),
        description: "A friendly app".into(),
        commands: BTreeMap::from([("hello".into(), "hello".into())]),
        source: None,
        app: None,
        desktop: None,
    }
}

#[tokio::test]
async fn private_cache_and_shared_index_preserve_original_publisher() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let publisher = Keys::generate();
    let operator = Keys::generate();
    let event = Announcement::sign(
        &publisher,
        &package(),
        "https://packages.example.org/catalog",
    )?;
    let cache = Discovery::open(&temp.path().join("operator"))?;
    assert_eq!(cache.ingest([event.clone()]).await?, 1);
    let out = temp.path().join("index");
    cache.export_index(&out, &operator).await?;
    let search_client = Discovery::open(&temp.path().join("search-reader"))?;
    assert_eq!(
        search_client
            .lookup_index(
                out.to_str().unwrap(),
                &operator.public_key().to_hex(),
                Some("friendly"),
                None
            )
            .await?,
        1
    );
    assert_eq!(search_client.announcements(None).await?[0].event, event);
    let client = Discovery::open(&temp.path().join("reader"))?;
    client
        .import_index(out.to_str().unwrap(), &operator.public_key().to_hex())
        .await?;
    let found = client.announcements(Some(publisher.public_key())).await?;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].event, event);
    assert!(
        client
            .announcements(Some(operator.public_key()))
            .await?
            .is_empty()
    );
    assert!(
        client
            .import_index(out.to_str().unwrap(), &publisher.public_key().to_hex())
            .await
            .is_err()
    );
    drop(client);
    let client = Discovery::open(&temp.path().join("reader"))?;
    assert_eq!(client.announcements(None).await?[0].event, event);
    let old = std::fs::read(out.join("index.json"))?;
    cache.export_index(&out, &operator).await?;
    assert_eq!(
        old,
        std::fs::read(out.join("index.json"))?,
        "unchanged export must not rewrite the public index"
    );
    let mut second = package();
    second.name = "second".into();
    cache
        .ingest([Announcement::sign(
            &publisher,
            &second,
            "https://packages.example.org/catalog",
        )?])
        .await?;
    cache.export_index(&out, &operator).await?;
    client
        .import_index(out.to_str().unwrap(), &operator.public_key().to_hex())
        .await?;
    std::fs::write(out.join("index.json"), old)?;
    assert!(
        client
            .import_index(out.to_str().unwrap(), &operator.public_key().to_hex())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn rejects_untrusted_paths_and_signatures() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let keys = Keys::generate();
    for location in [
        "/tmp/private",
        "file:///etc",
        "https://name:secret@example.com/catalog",
    ] {
        assert!(Announcement::sign(&keys, &package(), location).is_err());
    }
    let good = Announcement::sign(&keys, &package(), "https://example.com")?;
    let mut invalid = serde_json::to_value(&good)?;
    invalid["content"] = "forged".into();
    let invalid = serde_json::from_value(invalid)?;
    let cache = Discovery::open(temp.path())?;
    assert_eq!(cache.ingest([invalid]).await?, 0);
    let event = EventBuilder::new(haps::discovery::SOFTWARE_KIND, "")
        .tags([
            Tag::identifier("../oops"),
            Tag::parse(["name", "Oops"])?,
            Tag::parse(["haps_catalog", "https://example.com"])?,
        ])
        .sign_with_keys(&keys)?;
    assert_eq!(cache.ingest([event]).await?, 0);
    assert!(cache.announcements(None).await?.is_empty());
    Ok(())
}

#[derive(Clone)]
struct RelayState {
    events: std::sync::Arc<tokio::sync::Mutex<Vec<nostr::Event>>>,
    catalog: std::path::PathBuf,
    accept: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
async fn relay(
    axum::extract::State(state): axum::extract::State<RelayState>,
    upgrade: axum::extract::WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    upgrade.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        while let Some(Ok(message)) = socket.recv().await {
            let Message::Text(text) = message else {
                continue;
            };
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            if value[0] == "REQ" {
                for event in state.events.lock().await.iter() {
                    if socket
                        .send(Message::Text(
                            serde_json::json!(["EVENT", value[1], event]).to_string(),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                if socket
                    .send(Message::Text(
                        serde_json::json!(["EOSE", value[1]]).to_string(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
            } else if value[0] == "EVENT" {
                let event: nostr::Event = serde_json::from_value(value[1].clone()).unwrap();
                event.verify().unwrap();
                let accepted = state.accept.load(std::sync::atomic::Ordering::Relaxed);
                if accepted {
                    state.events.lock().await.push(event.clone());
                }
                if socket
                    .send(Message::Text(
                        serde_json::json!(["OK", event.id, accepted, ""]).to_string(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    })
}
async fn catalog_file(
    axum::extract::State(state): axum::extract::State<RelayState>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    if haps::model::safe_path(&path).is_err() {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    match std::fs::read(state.catalog.join(path)) {
        Ok(bytes) => bytes.into_response(),
        Err(_) => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn announcement_outbox_to_fresh_cli_install_without_source_registration() -> anyhow::Result<()>
{
    use nostr::nips::nip19::ToBech32;
    use std::{
        fs,
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    let temp = tempfile::tempdir()?;
    let publisher = Keys::generate();
    let payload = temp.path().join("payload");
    fs::create_dir_all(&payload)?;
    fs::write(payload.join("hello"), b"real verified payload")?;
    let catalog = temp.path().join("catalog");
    let repository = haps::repository::Repository::local(catalog.clone())?;
    let release = repository.publish(&publisher, package(), &payload).await?;
    let accept = Arc::new(AtomicBool::new(false));
    let state = RelayState {
        events: Arc::new(tokio::sync::Mutex::new(vec![])),
        catalog,
        accept: accept.clone(),
    };
    let app = axum::Router::new()
        .route("/ws", axum::routing::get(relay))
        .route("/catalog/*path", axum::routing::get(catalog_file))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let relay_url = url.replace("http:", "ws:") + "/ws";
    let bus = nostr_pubsub_relay::RelayEventBus::new([&relay_url], Duration::from_secs(1)).await?;
    let discovery = Discovery::open(&temp.path().join("publisher"))?;
    let announcement = Announcement::sign(&publisher, &package(), &(url + "/catalog"))?;
    discovery.queue(&[release.event.clone(), announcement])?;
    assert_eq!(
        discovery.flush(&bus).await?,
        2,
        "relay rejection must retain durable outbox"
    );
    accept.store(true, Ordering::Relaxed);
    assert_eq!(discovery.flush(&bus).await?, 0);
    let home = temp.path().join("fresh");
    let output = Command::new(env!("CARGO_BIN_EXE_haps"))
        .env("NOSTR_RELAYS", &relay_url)
        .env("HTREE_CONFIG_DIR", temp.path().join("htree"))
        .env("HTREE_PREFER_LOCAL_DAEMON", "false")
        .env("XDG_DATA_HOME", temp.path().join("data"))
        .args(["--home"])
        .arg(&home)
        .args([
            "--no-defaults",
            "--non-interactive",
            "install",
            &format!("{}/hello", publisher.public_key().to_bech32()?),
            "--allow-untrusted",
            "--json",
        ])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<serde_json::Value>(&output.stdout)?;
    let installation = haps::install::Installation::new(home)?.with_desktop_dir(None);
    assert_eq!(installation.receipt("hello")?.current, release.event);
    assert_eq!(
        fs::read(installation.command("hello", None)?)?,
        b"real verified payload"
    );
    task.abort();
    Ok(())
}
