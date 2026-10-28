use async_trait::async_trait;
use futures::StreamExt;
use haps::lookup::Lookup;
use nostr::{Event, EventBuilder, Filter, Keys, Kind};
use nostr_pubsub::*;
use std::{
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

struct StalledServer {
    url: String,
    accepted: Arc<AtomicBool>,
    finish: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}
impl StalledServer {
    fn new() -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}", listener.local_addr()?);
        let accepted = Arc::new(AtomicBool::new(false));
        let observed = accepted.clone();
        let (finish, finished) = mpsc::channel();
        let worker = thread::spawn(move || {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(3) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        observed.store(true, Ordering::SeqCst);
                        let _ = finished.recv_timeout(Duration::from_secs(3));
                        drop(socket);
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("fixture listener: {error}"),
                }
                if finished.try_recv().is_ok() {
                    return;
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
        Ok(Self {
            url,
            accepted,
            finish,
            worker: Some(worker),
        })
    }
}
impl Drop for StalledServer {
    fn drop(&mut self) {
        let _ = self.finish.send(());
        let _ = self.worker.take().unwrap().join();
    }
}

struct DelayedIndex {
    event: Event,
    ready: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl EventBus for DelayedIndex {
    async fn publish(&self, _: VerifiedEvent, _: EventSource) -> Result<PublishReport> {
        unreachable!()
    }
    async fn query(&self, _: Vec<Filter>, _: QueryOptions) -> Result<QueryReport> {
        self.ready.notified().await;
        Ok(QueryReport {
            events: vec![QueryEvent {
                event: self.event.clone().try_into()?,
                source: EventSource::local_index("fixture"),
                priority: 0,
            }],
        })
    }
}

#[tokio::test]
async fn independent_sources_deliver_before_slow_source_finishes_and_remain_additive()
-> anyhow::Result<()> {
    let keys = Keys::generate();
    let first = EventBuilder::new(Kind::TextNote, "first").sign_with_keys(&keys)?;
    let later = EventBuilder::new(Kind::TextNote, "later").sign_with_keys(&keys)?;
    let fast = Arc::new(tokio::sync::Notify::new());
    let slow = Arc::new(tokio::sync::Notify::new());
    let lookup = Lookup::default()
        .index(
            "fast",
            Arc::new(DelayedIndex {
                event: first.clone(),
                ready: fast.clone(),
            }),
        )?
        .index(
            "slow",
            Arc::new(DelayedIndex {
                event: later.clone(),
                ready: slow.clone(),
            }),
        )?;
    let mut updates = lookup.query_updates(vec![Filter::new().kind(Kind::TextNote)]);
    fast.notify_one();
    let batch = tokio::time::timeout(Duration::from_secs(1), updates.next())
        .await?
        .unwrap()?;
    assert_eq!(batch, vec![first]);
    // No EOSE or completion from the slow source was required for the first batch.
    slow.notify_one();
    let batch = tokio::time::timeout(Duration::from_secs(1), updates.next())
        .await?
        .unwrap()?;
    assert_eq!(batch, vec![later]);
    assert!(updates.next().await.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_remote_index_does_not_hold_up_cached_snapshot_results() -> anyhow::Result<()> {
    let server = StalledServer::new()?;
    let home = tempfile::tempdir()?;
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::TextNote, "cached").sign_with_keys(&keys)?;
    let ready = Arc::new(tokio::sync::Notify::new());
    ready.notify_one();
    let lookup = Lookup::default()
        .index(
            "cache",
            Arc::new(DelayedIndex {
                event: event.clone(),
                ready,
            }),
        )?
        .index(
            "remote",
            Arc::new(
                haps::index_reader::IndexReader::new(
                    home.path(),
                    &server.url,
                    &keys.public_key().to_hex(),
                )
                .with_timeout(Duration::from_millis(500)),
            ),
        )?;
    let started = Instant::now();
    let events = lookup
        .query(vec![Filter::new().kind(Kind::TextNote)])
        .await?;
    assert_eq!(events, vec![event]);
    assert!(server.accepted.load(Ordering::SeqCst));
    assert!(started.elapsed() < Duration::from_secs(2));
    Ok(())
}

#[test]
fn cancelling_a_query_does_not_leave_its_blocking_reader_waiting_on_network() -> anyhow::Result<()>
{
    let server = StalledServer::new()?;
    let home = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()?;
    let reader = haps::index_reader::IndexReader::new(
        home.path(),
        &server.url,
        &Keys::generate().public_key().to_hex(),
    )
    .with_timeout(Duration::from_secs(1));
    let (cancelled, reader_finished) = runtime.block_on(async {
        let query = tokio::spawn(async move {
            reader
                .query(vec![Filter::new()], QueryOptions::default())
                .await
        });
        // Cancel only after the network read actually starts, rather than
        // relying on the runner scheduling it within a fixed short sleep.
        while !server.accepted.load(Ordering::SeqCst) && !query.is_finished() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        query.abort();
        let cancelled = query.await.is_err_and(|error| error.is_cancelled());
        // The sentinel can run only when the sole blocking worker is free.
        let sentinel = tokio::task::spawn_blocking(|| ());
        let finished = tokio::time::timeout(Duration::from_secs(2), sentinel)
            .await
            .is_ok();
        (cancelled, finished)
    });
    let connected = server.accepted.load(Ordering::SeqCst);
    // Release the socket before assertions so even the regression closes cleanly.
    drop(server);
    drop(runtime);
    assert!(connected && cancelled);
    assert!(
        reader_finished,
        "cancelled index query kept its blocking worker"
    );
    Ok(())
}
