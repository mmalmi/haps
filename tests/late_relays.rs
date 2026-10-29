use anyhow::Context;
use axum::{
    extract::{State, WebSocketUpgrade, ws::Message},
    response::IntoResponse,
};
use futures::StreamExt;
use haps::{
    discovery::{Announcement, SOFTWARE_KIND},
    lookup::Lookup,
    model::{PackageSpec, target},
    repository::Repository,
};
use nostr::{Event, Filter, Keys, Timestamp};
use nostr_pubsub_relay::RelayEventBus;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Notify};

#[derive(Clone)]
struct StateData {
    gate: Option<Arc<Notify>>,
    attempted: Arc<Notify>,
    events: Vec<Event>,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

async fn relay(State(state): State<StateData>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
    state.attempted.notify_one();
    if let Some(gate) = &state.gate {
        gate.notified().await;
    }
    upgrade.on_upgrade(move |mut socket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            let Message::Text(text) = message else {
                continue;
            };
            let request: serde_json::Value = serde_json::from_str(&text).unwrap();
            state.requests.lock().await.push(request.clone());
            if request[0] != "REQ" {
                continue;
            }
            let filters: Vec<Filter> = request.as_array().unwrap()[2..]
                .iter()
                .map(|value| serde_json::from_value(value.clone()).unwrap())
                .collect();
            for event in &state.events {
                if filters
                    .iter()
                    .any(|filter| filter.match_event(event, Default::default()))
                    && socket
                        .send(Message::Text(
                            serde_json::json!(["EVENT", request[1], event]).to_string(),
                        ))
                        .await
                        .is_err()
                {
                    return;
                }
            }
            if socket
                .send(Message::Text(
                    serde_json::json!(["EOSE", request[1]]).to_string(),
                ))
                .await
                .is_err()
            {
                return;
            }
        }
    })
}

struct TestRelay {
    url: String,
    state: StateData,
    worker: tokio::task::JoinHandle<()>,
}
impl TestRelay {
    async fn start(events: Vec<Event>, gated: bool) -> anyhow::Result<Self> {
        let state = StateData {
            gate: gated.then(|| Arc::new(Notify::new())),
            attempted: Arc::new(Notify::new()),
            events,
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}", listener.local_addr()?);
        let app = axum::Router::new()
            .route("/", axum::routing::get(relay))
            .with_state(state.clone());
        let worker = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Ok(Self { url, state, worker })
    }
}
impl Drop for TestRelay {
    fn drop(&mut self) {
        if let Some(gate) = &self.state.gate {
            gate.notify_waiters();
        }
        self.worker.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_fast_head_does_not_exclude_late_release_or_wait_for_stalled_relay()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let keys = Keys::generate();
    let payload = temp.path().join("payload");
    std::fs::create_dir(&payload)?;
    std::fs::write(payload.join("hello"), b"fixture")?;
    let repo = Repository::local(temp.path().join("catalog"))?;
    let mut package = PackageSpec {
        name: "hello".into(),
        version: "1.0.0".parse()?,
        target: target().into(),
        description: "Late relay fixture".into(),
        commands: BTreeMap::from([("hello".into(), "hello".into())]),
        dependencies: vec![],
        source: None,
        app: None,
        desktop: None,
    };
    let old_release = repo.publish(&keys, package.clone(), &payload).await?;
    let snapshot = repo.catalog(&keys.public_key().to_hex()).await?;
    let old_head = Announcement::sign_direct(
        &keys,
        &repo,
        &snapshot,
        "hello",
        "https://example.test/catalog",
    )
    .await?;
    // Give the two heads an explicit order, independent of wall-clock ticks.
    let old_head = nostr::EventBuilder::new(SOFTWARE_KIND, old_head.content)
        .tags(old_head.tags)
        .custom_created_at(Timestamp::from(
            Timestamp::now().as_secs().saturating_sub(1),
        ))
        .sign_with_keys(&keys)?;
    package.version = "2.0.0".parse()?;
    let release = repo.publish(&keys, package, &payload).await?;
    let snapshot = repo.catalog(&keys.public_key().to_hex()).await?;
    let head = Announcement::sign_direct(
        &keys,
        &repo,
        &snapshot,
        "hello",
        "https://example.test/catalog",
    )
    .await?;
    let fast = TestRelay::start(vec![old_head.clone(), old_release.event], false).await?;
    let later = TestRelay::start(vec![head.clone(), release.event.clone()], true).await?;
    let stalled = TestRelay::start(vec![], true).await?;
    let bus = Arc::new(
        RelayEventBus::new(
            [&fast.url, &later.url, &stalled.url],
            Duration::from_secs(3),
        )
        .await?,
    );
    let fast_relay = bus.client().relay(&fast.url).await?;
    fast_relay.wait_for_connection(Duration::from_secs(2)).await;
    assert!(fast_relay.is_connected());
    tokio::time::timeout(Duration::from_secs(2), later.state.attempted.notified()).await?;
    tokio::time::timeout(Duration::from_secs(2), stalled.state.attempted.notified()).await?;
    assert!(!bus.client().relay(&later.url).await?.is_connected());
    let lookup = Lookup::default().relays(bus.clone(), Duration::from_secs(3))?;
    let filter = Filter::new()
        .kind(SOFTWARE_KIND)
        .author(keys.public_key())
        .identifier("hello");
    let mut updates = lookup.query_updates(vec![filter.clone()]);
    let started = tokio::time::Instant::now();
    let first = tokio::time::timeout(Duration::from_secs(1), updates.next())
        .await
        .context("connected relay blocked behind unfinished handshakes")?
        .expect("connected relay must deliver before other handshakes finish")?;
    assert_eq!(first, vec![old_head]);
    // The later relay can only finish its handshake after the first result.
    // A ready-relay snapshot misses it; joining setup on all relays blocks first.
    later.state.gate.as_ref().unwrap().notify_one();
    let second = tokio::time::timeout(Duration::from_secs(1), updates.next())
        .await
        .context("late relay did not contribute before the deadline")?
        .expect("late relay must contribute within the same lookup")?;
    assert_eq!(second, vec![head.clone()]);
    while let Some(batch) =
        tokio::time::timeout_at(started + Duration::from_secs(4), updates.next()).await?
    {
        assert!(batch.is_err() || batch?.is_empty(), "unexpected result");
    }
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(!bus.client().relay(&stalled.url).await?.is_connected());

    // Fetch the exact release advertised by the later head, with the same
    // ordinary anonymous query interface used for package and social records.
    let mut updates = lookup.query_updates(vec![Filter::new().id(release.event.id)]);
    let batch = tokio::time::timeout(Duration::from_secs(1), updates.next())
        .await?
        .unwrap()?;
    let announcement = Announcement::verify(head)?;
    let head = announcement.head.unwrap();
    let pointer = head
        .releases
        .iter()
        .find(|p| p.id == release.event.id.to_hex())
        .unwrap();
    assert_eq!(
        head.verify_release(
            pointer,
            batch[0].clone(),
            &keys.public_key().to_hex(),
            "hello"
        )?
        .event,
        release.event
    );
    drop(updates);
    bus.client().shutdown().await;
    for server in [&fast, &later, &stalled] {
        for request in server.state.requests.lock().await.iter() {
            assert!(
                request[0] == "REQ" || request[0] == "CLOSE",
                "lookup must not authenticate or publish"
            );
            if request[0] == "REQ" {
                let actual: Filter = serde_json::from_value(request[2].clone())?;
                assert!(
                    actual == filter || actual == Filter::new().id(release.event.id),
                    "query scope widened"
                );
            }
        }
    }
    Ok(())
}
