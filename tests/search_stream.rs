use haps::{
    discovery::{Announcement, Discovery},
    model::{PackageSpec, target},
    repository::Repository,
};
use nostr::Keys;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    root: tempfile::TempDir,
    home: PathBuf,
    release_id: String,
    _listener: std::net::TcpListener,
}
impl Fixture {
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_haps"));
        command
            .args(["--no-defaults", "--home"])
            .arg(&self.home)
            .args(args)
            .env_remove("NOSTR_RELAYS")
            .env("HTREE_CONFIG_DIR", self.root.path().join("htree"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

fn wait_for_exit(child: &mut ChildGuard) -> anyhow::Result<ExitStatus> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        anyhow::ensure!(
            started.elapsed() < Duration::from_secs(6),
            "metadata lookup kept the command waiting"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn fixture(cache_release: bool) -> anyhow::Result<Fixture> {
    let root = tempfile::tempdir()?;
    let home = root.path().join("reader");
    let publisher = Keys::generate();
    let payload = root.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("data"), "fixture")?;
    let repo = Repository::local(root.path().join("repo"))?;
    let release = repo
        .publish(
            &publisher,
            PackageSpec {
                name: "hello".into(),
                version: "1.0.0".parse()?,
                target: target().into(),
                description: "Cached friendly package".into(),
                commands: BTreeMap::new(),
                dependencies: vec![],
                source: None,
                app: None,
                desktop: None,
            },
            &payload,
        )
        .await?;
    let snapshot = repo.catalog(&publisher.public_key().to_hex()).await?;
    // Keep several HTTP requests unanswered to detect cumulative per-source waits.
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let location = format!("http://{}", listener.local_addr()?);
    let head = Announcement::sign_direct(&publisher, &repo, &snapshot, "hello", &location).await?;
    let discovery = Discovery::open(&home)?;
    discovery.ingest([head]).await?;
    if cache_release {
        discovery.ingest([release.event.clone()]).await?;
    }
    drop(discovery);

    let other = Keys::generate();
    let sources: BTreeMap<_, _> = (0..4)
        .map(|i| {
            (
                format!("slow-{i}"),
                serde_json::json!({
                    "location": location, "author": other.public_key().to_hex(),
                    "sequence": 0, "event_id": ""
                }),
            )
        })
        .collect();
    fs::write(
        home.join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "identity": publisher.public_key().to_hex(),
            "sources": sources,
            "indexes": {}, "starting_point": null, "discovery_defaults_version": 2
        }))?,
    )?;
    Ok(Fixture {
        root,
        home,
        release_id: release.event.id.to_hex(),
        _listener: listener,
    })
}

#[tokio::test]
async fn cached_snapshot_is_flushed_before_an_unresponsive_source_finishes() -> anyhow::Result<()> {
    let fixture = fixture(true).await?;
    let mut child = ChildGuard(
        fixture
            .command(&["search", "--json", "--stream", "--", "friendly"])
            .spawn()?,
    );
    let output = child.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(output).read_line(&mut line).map(|_| line);
        let _ = send.send(result);
    });
    let line = receive.recv_timeout(Duration::from_secs(5))??;
    let rows: serde_json::Value = serde_json::from_str(&line)?;
    assert_eq!(rows[0]["release_id"], fixture.release_id);
    assert!(
        child.0.try_wait()?.is_none(),
        "the source should still be pending"
    );
    assert!(wait_for_exit(&mut child)?.success());
    reader.join().unwrap();
    Ok(())
}

#[tokio::test]
async fn snapshot_search_and_info_finish_with_cached_results_despite_stalled_catalogs()
-> anyhow::Result<()> {
    let fixture = fixture(true).await?;
    for args in [
        vec!["search", "--json", "friendly"],
        vec!["info", "hello", "--json"],
    ] {
        let mut child = ChildGuard(fixture.command(&args).spawn()?);
        assert!(wait_for_exit(&mut child)?.success());
        let output = child.0.stdout.take().unwrap();
        let response: serde_json::Value = serde_json::from_reader(output)?;
        assert!(serde_json::to_string(&response)?.contains(&fixture.release_id));
    }
    Ok(())
}

#[tokio::test]
async fn missing_release_metadata_uses_the_short_query_deadline() -> anyhow::Result<()> {
    let fixture = fixture(false).await?;
    let mut child = ChildGuard(fixture.command(&["search", "--json", "friendly"]).spawn()?);
    assert!(wait_for_exit(&mut child)?.success());
    let response: serde_json::Value = serde_json::from_reader(child.0.stdout.take().unwrap())?;
    assert_eq!(response, serde_json::json!([]));
    Ok(())
}
