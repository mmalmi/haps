use axum::{
    Router,
    extract::{Path as RequestPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use haps::{
    install::Installation,
    model::{Manifest, PackageSpec, Release, target},
    repository::Repository,
};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, mpsc};

const FILES: usize = 12;

#[derive(Clone)]
struct ServerState {
    repository: PathBuf,
    payload_paths: Arc<BTreeMap<String, usize>>,
    gates: Arc<Vec<Semaphore>>,
    started: mpsc::UnboundedSender<usize>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    corrupt: Arc<AtomicUsize>,
}

struct Active(Arc<AtomicUsize>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn blob(
    State(state): State<ServerState>,
    RequestPath(path): RequestPath<String>,
) -> Response {
    let _active = if let Some(&index) = state.payload_paths.get(&path) {
        let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak.fetch_max(active, Ordering::SeqCst);
        let active = Active(state.active.clone());
        let _ = state.started.send(index);
        let Ok(permit) = state.gates[index].acquire().await else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        permit.forget();
        if state.corrupt.load(Ordering::SeqCst) == index {
            return b"corrupted payload".to_vec().into_response();
        }
        Some(active)
    } else {
        None
    };
    match fs::read(state.repository.join(path)) {
        Ok(bytes) => bytes.into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

struct Fixture {
    temporary: tempfile::TempDir,
    remote: Repository,
    release: Release,
    manifest: Manifest,
    state: ServerState,
    started: mpsc::UnboundedReceiver<usize>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new() -> anyhow::Result<Self> {
        let temporary = tempfile::tempdir()?;
        let payload = temporary.path().join("payload");
        fs::create_dir_all(payload.join("files"))?;
        for index in 0..FILES {
            fs::write(payload.join(format!("files/{index:02}")), contents(index))?;
        }
        let repository = temporary.path().join("repository");
        let local = Repository::local(repository.clone())?;
        let release = local
            .publish(
                &nostr::Keys::generate(),
                PackageSpec {
                    name: "parallel-fixture".into(),
                    version: "1.0.0".parse()?,
                    target: target().into(),
                    description: "Controlled payload transport".into(),
                    commands: BTreeMap::from([("fixture".into(), "files/00".into())]),
                    dependencies: vec![],
                    source: None,
                    app: None,
                    desktop: None,
                },
                &payload,
            )
            .await?;
        let manifest: Manifest = local.json(&release.data.manifest).await?;
        let payload_paths = manifest
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let cid = hashtree_core::Cid::parse(&file.cid).unwrap();
                (
                    format!(
                        "blobs/{}",
                        haps::store::block_path(&cid.hash)
                            .to_string_lossy()
                            .replace('\\', "/")
                    ),
                    index,
                )
            })
            .collect();
        let (sender, started) = mpsc::unbounded_channel();
        let state = ServerState {
            repository,
            payload_paths: Arc::new(payload_paths),
            gates: Arc::new((0..FILES).map(|_| Semaphore::new(0)).collect()),
            started: sender,
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            corrupt: Arc::new(AtomicUsize::new(usize::MAX)),
        };
        let app = Router::new()
            .route("/*path", get(blob))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let remote = Repository::open(
            &format!("http://{}", listener.local_addr()?),
            &temporary.path().join("cache"),
        )?;
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Ok(Self {
            temporary,
            remote,
            release,
            manifest,
            state,
            started,
            server,
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for gate in self.state.gates.iter() {
            gate.close();
        }
        self.server.abort();
    }
}

fn contents(index: usize) -> Vec<u8> {
    vec![index as u8 + 1; 64 * 1024 + index]
}

#[tokio::test]
async fn independent_payload_files_overlap_with_a_fixed_bound() -> anyhow::Result<()> {
    let mut fixture = Fixture::new().await?;
    let installed =
        Installation::new(fixture.temporary.path().join("home"))?.with_desktop_dir(None);
    let start = Instant::now();
    let download = installed.install(&fixture.remote, &fixture.release);
    tokio::pin!(download);
    // No file can finish until four real HTTP requests have arrived. The old
    // serial implementation times out here, independently of machine speed.
    for _ in 0..4 {
        tokio::select! {
            result = &mut download => panic!("install finished before downloads overlapped: {result:?}"),
            request = tokio::time::timeout(Duration::from_secs(5), fixture.started.recv()) => {
                request.expect("payload downloads remained serial").expect("server stopped");
            }
        }
    }
    assert_eq!(fixture.state.active.load(Ordering::SeqCst), 4);
    for gate in fixture.state.gates.iter() {
        gate.add_permits(1);
    }
    tokio::time::timeout(Duration::from_secs(5), &mut download).await??;
    assert_eq!(fixture.state.peak.load(Ordering::SeqCst), 4);
    let directory = installed.path("parallel-fixture")?;
    for file in &fixture.manifest.files {
        let index: usize = file.path.rsplit('/').next().unwrap().parse()?;
        let path = directory.join(&file.path);
        assert_eq!(fs::read(&path)?, contents(index));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path)?.permissions().mode() & 0o777,
                if file.executable { 0o755 } else { 0o644 }
            );
        }
    }
    println!(
        "Verified {FILES} HTTP payloads with four overlapping reads in {:?}",
        start.elapsed()
    );
    Ok(())
}

#[tokio::test]
async fn corrupt_file_cancels_pending_writers_before_staging_cleanup() -> anyhow::Result<()> {
    let mut fixture = Fixture::new().await?;
    let home = fixture.temporary.path().join("home");
    let installed = Installation::new(home.clone())?.with_desktop_dir(None);
    let download = installed.install(&fixture.remote, &fixture.release);
    tokio::pin!(download);
    let mut requested = Vec::new();
    for _ in 0..4 {
        tokio::select! {
            result = &mut download => panic!("install finished before failure injection: {result:?}"),
            request = tokio::time::timeout(Duration::from_secs(5), fixture.started.recv()) => {
                requested.push(request?.expect("server stopped"));
            }
        }
    }
    fixture.state.corrupt.store(requested[0], Ordering::SeqCst);
    fixture.state.gates[requested[0]].add_permits(1);
    let error = tokio::time::timeout(Duration::from_secs(2), &mut download)
        .await
        .expect("failed install waited for unrelated payloads")
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("content hash mismatch"),
        "{error:#}"
    );
    assert!(installed.receipts()?.is_empty());
    assert!(!home.join("installed.json").exists());
    let parent = home
        .join("packages")
        .join(fixture.release.author())
        .join("parallel-fixture");
    assert_eq!(
        fs::read_dir(&parent)?.count(),
        0,
        "failed stage was retained"
    );
    // Let the remote handlers finish after the client has failed. They must
    // have no surviving writer capable of recreating the discarded stage.
    for gate in fixture.state.gates.iter() {
        gate.close();
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.state.active.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(fs::read_dir(parent)?.count(), 0);
    assert!(
        fixture.started.try_recv().is_err(),
        "new downloads began after failure"
    );
    Ok(())
}
