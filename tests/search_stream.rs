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
    process::{Child, Command, Stdio},
    time::Duration,
};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn cached_snapshot_is_flushed_before_an_unresponsive_source_finishes() -> anyhow::Result<()> {
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
    let head = Announcement::sign_direct(
        &publisher,
        &repo,
        &snapshot,
        "hello",
        "https://unneeded.example.org",
    )
    .await?;
    let discovery = Discovery::open(&home)?;
    discovery.ingest([head, release.event.clone()]).await?;
    drop(discovery);

    // Hold a local listener open without serving its HTTP request. The first
    // snapshot must not depend on this legacy source responding or timing out.
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    fs::write(
        home.join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "identity": publisher.public_key().to_hex(),
            "sources": {"slow": {
                "location": format!("http://{}", listener.local_addr()?),
                "author": publisher.public_key().to_hex(), "sequence": 0, "event_id": ""
            }},
            "indexes": {}, "starting_point": null, "discovery_defaults_version": 2
        }))?,
    )?;
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_haps"))
            .args(["--no-defaults", "--home"])
            .arg(&home)
            .args(["search", "--json", "--stream", "--", "friendly"])
            .env_remove("NOSTR_RELAYS")
            .env("HTREE_CONFIG_DIR", root.path().join("htree"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
    assert_eq!(rows[0]["release_id"], release.event.id.to_hex());
    assert!(
        child.0.try_wait()?.is_none(),
        "the source should still be pending"
    );
    drop(child);
    reader.join().unwrap();
    Ok(())
}
