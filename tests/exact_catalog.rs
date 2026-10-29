use haps::{
    model::{PackageSpec, Release, ReleaseData, sign, sign_release, target},
    repository::{Catalog, CatalogRoot, Repository},
    store::block_path,
};
use hashtree_core::Cid;
use hashtree_index::{BTree, BTreeOptions};
use nostr::Keys;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

struct Fixture {
    temp: tempfile::TempDir,
    repo: Repository,
    keys: Keys,
    release: Release,
    cid: String,
    catalog: Catalog,
}

impl Fixture {
    async fn new(name: &str) -> anyhow::Result<Self> {
        let temp = tempfile::tempdir()?;
        let repo = Repository::local(temp.path().join("repo"))?;
        let keys = Keys::generate();
        let data = ReleaseData {
            schema: "haps.release.v1".into(),
            package: PackageSpec {
                name: name.into(),
                version: "2026.10.2401".parse()?,
                target: target().into(),
                description: "Exact catalog fixture".into(),
                commands: BTreeMap::new(),
                dependencies: vec![],
                source: None,
                app: None,
                desktop: None,
            },
            manifest: unavailable_cid(999),
        };
        let release = Release::verify(sign_release(&keys, &data)?)?;
        let cid = repo.put_json(&release.event).await?;
        // More than the ordinary search limit, with unavailable records before
        // the requested version. Their index metadata is still authenticated.
        let mut catalog = Catalog::default();
        let mut entries = Vec::new();
        for i in 0..150 {
            let missing = unavailable_cid(i);
            entries.push((
                format!("{name}:{}/{}@1.0.{i}#{}", release.author(), name, target()),
                missing.clone(),
            ));
            catalog.releases.push(missing);
        }
        catalog.releases.push(cid.clone());
        for keyword in name.split('-') {
            entries.push((format!("{keyword}:{}", release.coordinate()), cid.clone()));
        }
        catalog.search = BTree::new(repo.store.clone(), BTreeOptions::default())
            .build(entries)
            .await?
            .map(|root| root.to_string());
        let fixture = Self {
            temp,
            repo,
            keys,
            release,
            cid,
            catalog,
        };
        fixture.write_catalog(&fixture.catalog).await?;
        Ok(fixture)
    }

    async fn write_catalog(&self, catalog: &Catalog) -> anyhow::Result<()> {
        let head = CatalogRoot {
            schema: "haps.catalog.v1".into(),
            sequence: 1,
            root: self.repo.put_json(catalog).await?,
        };
        fs::write(
            self.repo.path().unwrap().join("catalog.json"),
            serde_json::to_vec(&sign(&self.keys, "haps/catalog/v1", &head)?)?,
        )?;
        Ok(())
    }

    async fn exact(&self) -> anyhow::Result<Option<Release>> {
        let snapshot = self.repo.catalog(&self.release.author()).await?;
        self.repo
            .exact_release(
                &snapshot,
                &self.release.identity(),
                &self.release.data.package.version,
                target(),
            )
            .await
    }

    async fn point_to(&mut self, event: &nostr::Event) -> anyhow::Result<()> {
        let cid = self.repo.put_json(event).await?;
        self.catalog.releases.push(cid.clone());
        self.catalog.search = BTree::new(self.repo.store.clone(), BTreeOptions::default())
            .build([(format!("iris:{}", self.release.coordinate()), cid)])
            .await?
            .map(|root| root.to_string());
        self.write_catalog(&self.catalog).await
    }
}

fn unavailable_cid(i: usize) -> String {
    let hash = hex::encode(Sha256::digest(format!("unavailable fixture {i}")));
    format!("{hash}:{hash}")
}

#[tokio::test]
async fn selected_record_must_be_present_authentic_and_match_the_index() -> anyhow::Result<()> {
    let mut fixture = Fixture::new("iris").await?;
    assert_eq!(
        fixture.exact().await?.unwrap().event.id,
        fixture.release.event.id
    );

    let path = fixture
        .repo
        .path()
        .unwrap()
        .join("blobs")
        .join(block_path(&Cid::parse(&fixture.cid)?.hash));
    let original = fs::read(&path)?;
    fs::remove_file(&path)?;
    assert!(
        format!("{:#}", fixture.exact().await.unwrap_err())
            .contains("selected release metadata unavailable")
    );
    fs::write(&path, b"corrupt")?;
    assert!(format!("{:#}", fixture.exact().await.unwrap_err()).contains("content hash mismatch"));
    fs::write(path, original)?;

    let mut forged = serde_json::to_value(&fixture.release.event)?;
    forged["content"] = "{}".into();
    fixture.point_to(&serde_json::from_value(forged)?).await?;
    assert!(
        format!("{:#}", fixture.exact().await.unwrap_err())
            .contains("invalid Nostr event signature or ID")
    );

    let wrong_author = sign_release(&Keys::generate(), &fixture.release.data)?;
    fixture.point_to(&wrong_author).await?;
    assert!(
        fixture
            .exact()
            .await
            .unwrap_err()
            .to_string()
            .contains("search identity mismatch")
    );
    for mismatch in ["version", "target", "name"] {
        let mut data = fixture.release.data.clone();
        match mismatch {
            "version" => data.package.version = "2026.10.2402".parse()?,
            "target" => data.package.target = "other-platform".into(),
            _ => data.package.name = "other".into(),
        }
        fixture
            .point_to(&sign_release(&fixture.keys, &data)?)
            .await?;
        assert!(
            fixture
                .exact()
                .await
                .unwrap_err()
                .to_string()
                .contains("search identity mismatch")
        );
    }
    Ok(())
}

#[tokio::test]
async fn exact_index_preserves_catalog_membership_and_conflict_checks() -> anyhow::Result<()> {
    let mut fixture = Fixture::new("iris-chat").await?;
    let original = fixture.catalog.clone();
    fixture.catalog.releases.retain(|cid| cid != &fixture.cid);
    fixture.write_catalog(&fixture.catalog).await?;
    assert!(
        fixture
            .exact()
            .await
            .unwrap_err()
            .to_string()
            .contains("outside its catalog")
    );
    fixture.catalog = original.clone();
    fixture.catalog.releases.push(fixture.cid.clone());
    fixture.write_catalog(&fixture.catalog).await?;
    assert!(
        fixture
            .exact()
            .await
            .unwrap_err()
            .to_string()
            .contains("duplicate release")
    );
    fixture.catalog = original;
    let tree = BTree::new(fixture.repo.store.clone(), BTreeOptions::default());
    fixture.catalog.search = Some(
        tree.insert(
            Some(&Cid::parse(fixture.catalog.search.as_ref().unwrap())?),
            &format!("chat:{}", fixture.release.coordinate()),
            &fixture.catalog.releases[0],
        )
        .await?
        .to_string(),
    );
    fixture.write_catalog(&fixture.catalog).await?;
    assert!(
        fixture
            .exact()
            .await
            .unwrap_err()
            .to_string()
            .contains("conflicting release coordinates")
    );
    Ok(())
}

#[tokio::test]
async fn legacy_catalog_without_index_keeps_bounded_scan_compatibility() -> anyhow::Result<()> {
    let mut fixture = Fixture::new("iris").await?;
    fixture.catalog.search = None;
    fixture.catalog.releases = vec![fixture.cid.clone()];
    fixture.write_catalog(&fixture.catalog).await?;
    assert_eq!(
        fixture.exact().await?.unwrap().event.id,
        fixture.release.event.id
    );
    Ok(())
}

struct Server {
    url: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new(root: PathBuf, stall_missing: bool) -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = requests.clone();
        let worker = std::thread::spawn(move || {
            let mut stalled = Vec::new();
            while !stopped.load(Ordering::Relaxed) {
                if let Some(request) = server.recv_timeout(Duration::from_millis(10)).unwrap() {
                    let path = request.url().trim_start_matches('/');
                    received.lock().unwrap().push(path.to_string());
                    if let Ok(path) = haps::model::safe_path(path)
                        && let Ok(file) = fs::File::open(root.join(path))
                    {
                        let _ = request.respond(tiny_http::Response::from_file(file));
                    } else if stall_missing {
                        stalled.push(request);
                    } else {
                        let _ = request.respond(tiny_http::Response::empty(404));
                    }
                }
            }
        });
        Self {
            url,
            stop,
            requests,
            worker: Some(worker),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn info(fixture: &Fixture, location: &str, label: &str) -> anyhow::Result<(bool, String)> {
    let home = fixture.temp.path().join(label);
    fs::create_dir_all(&home)?;
    fs::write(
        home.join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "identity": fixture.release.author(),
            "sources": {"fixture": {"location": location, "author": fixture.release.author(), "sequence": 0, "event_id": ""}},
            "indexes": {}, "starting_point": null, "discovery_defaults_version": 2
        }))?,
    )?;
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_haps"))
            .args(["--no-defaults", "--home"])
            .arg(&home)
            .args([
                "info",
                &fixture.release.identity(),
                "--version",
                "2026.10.2401",
                "--json",
            ])
            .env_remove("NOSTR_RELAYS")
            .env("HTREE_CONFIG_DIR", fixture.temp.path().join("htree"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        anyhow::ensure!(
            started.elapsed() < Duration::from_secs(6),
            "exact lookup exceeded its deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut output = String::new();
    std::io::Read::read_to_string(&mut child.0.stdout.take().unwrap(), &mut output)?;
    Ok((status.success(), output))
}

#[tokio::test]
async fn exact_cli_ignores_unavailable_unrelated_releases_but_fails_on_selected_one()
-> anyhow::Result<()> {
    let fixture = Fixture::new("iris").await?;
    let requested = format!(
        "blobs/{}",
        block_path(&Cid::parse(&fixture.cid)?.hash)
            .to_string_lossy()
            .replace('\\', "/")
    );
    let unrelated = format!(
        "blobs/{}",
        block_path(&Cid::parse(&fixture.catalog.releases[0])?.hash)
            .to_string_lossy()
            .replace('\\', "/")
    );
    for stall in [false, true] {
        let server = Server::new(fixture.repo.path().unwrap().into(), stall);
        let (success, output) = info(&fixture, &server.url, &format!("reader-{stall}"))?;
        assert!(
            success,
            "exact CLI must ignore unrelated unavailable records: {output}"
        );
        assert!(output.contains(&fixture.release.event.id.to_hex()));
        let requests = server.requests.lock().unwrap();
        assert!(requests.contains(&requested));
        assert!(!requests.contains(&unrelated));
        assert!(
            requests.len() < 16,
            "lookup fetched unrelated index/release records"
        );
    }
    fs::remove_file(fixture.repo.path().unwrap().join(&requested))?;
    let server = Server::new(fixture.repo.path().unwrap().into(), true);
    let (success, output) = info(&fixture, &server.url, "missing-selected")?;
    assert!(!success);
    assert!(output.contains("unavailable or incomplete"), "{output}");
    assert!(server.requests.lock().unwrap().contains(&requested));
    Ok(())
}

#[tokio::test]
async fn exact_direct_head_skips_other_versions_and_targets() -> anyhow::Result<()> {
    use haps::{
        discovery::Discovery,
        package_head::{PackageHead, ReleasePointer},
    };
    use nostr::{EventBuilder, Tag};
    let fixture = Fixture::new("iris").await?;
    let server = Server::new(fixture.repo.path().unwrap().into(), true);
    let pointer = ReleasePointer {
        id: fixture.release.event.id.to_hex(),
        cid: fixture.cid.clone(),
        version: fixture.release.data.package.version.clone(),
        target: target().into(),
    };
    let mut other_version = pointer.clone();
    other_version.version = "1.0.0".parse()?;
    other_version.cid = unavailable_cid(200);
    let mut other_target = pointer.clone();
    other_target.target = "other-platform".into();
    other_target.cid = unavailable_cid(201);
    let head = PackageHead {
        schema: "haps.package-head.v1".into(),
        description: "Exact pointer fixture".into(),
        payload: server.url.clone(),
        card: unavailable_cid(202),
        releases: vec![other_version, other_target, pointer],
    };
    let event = EventBuilder::new(
        haps::discovery::SOFTWARE_KIND,
        serde_json::to_string(&head)?,
    )
    .tags([
        Tag::identifier("iris"),
        Tag::parse(["name", "iris"])?,
        Tag::parse(["haps_head", "1"])?,
    ])
    .sign_with_keys(&fixture.keys)?;
    let discovery = Discovery::open(&fixture.temp.path().join("direct"))?;
    discovery.ingest([event]).await?;
    drop(discovery);
    let (success, output) = info(&fixture, &server.url, "direct")?;
    assert!(
        success,
        "exact direct head must skip unrelated pointers: {output}"
    );
    assert!(output.contains(&fixture.release.event.id.to_hex()));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    Ok(())
}

#[tokio::test]
async fn missing_local_index_value_is_unavailable_and_does_not_advance_checkpoint()
-> anyhow::Result<()> {
    let mut fixture = Fixture::new("iris").await?;
    let root = BTree::new(fixture.repo.store.clone(), BTreeOptions::default())
        .build([(
            format!("iris:{}", fixture.release.coordinate()),
            fixture.cid.clone(),
        )])
        .await?
        .unwrap();
    fixture.catalog.search = Some(root.to_string());
    fixture.write_catalog(&fixture.catalog).await?;
    let snapshot = fixture.repo.catalog(&fixture.release.author()).await?;
    // A truly absent key in an intact authenticated leaf remains distinguishable.
    assert!(
        fixture
            .repo
            .exact_release(
                &snapshot,
                &fixture.release.identity(),
                &"1.2.3".parse()?,
                target()
            )
            .await?
            .is_none()
    );
    let entries = fixture.repo.store.tree().list_directory(&root).await?;
    assert_eq!(entries.len(), 1);
    fs::remove_file(
        fixture
            .repo
            .path()
            .unwrap()
            .join("blobs")
            .join(block_path(&entries[0].hash)),
    )?;
    let result = fixture.exact().await;
    assert!(
        result.is_err(),
        "missing referenced index value must be unavailable, not an absent key"
    );
    assert!(
        format!("{:#}", result.unwrap_err()).contains("referenced catalog index block unavailable")
    );
    let (success, output) = info(
        &fixture,
        fixture.repo.path().unwrap().to_str().unwrap(),
        "missing-index-value",
    )?;
    assert!(!success);
    assert!(output.contains("unavailable or incomplete"), "{output}");
    let config: serde_json::Value = serde_json::from_slice(&fs::read(
        fixture.temp.path().join("missing-index-value/config.json"),
    )?)?;
    assert_eq!(config["sources"]["fixture"]["sequence"], 0);
    assert_eq!(config["sources"]["fixture"]["event_id"], "");
    Ok(())
}
