#[path = "support/audit.rs"]
mod audit;
use haps::{
    model::{Manifest, PackageSpec, target},
    repository::Repository,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

struct Running {
    child: Child,
    lines: mpsc::Receiver<String>,
    stderr: String,
}

impl Running {
    fn start(home: &Path, args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_haps"))
            .env("HAPS_HOME", home)
            .env("HAPS_NO_DEFAULTS", "true")
            .env("HTREE_CONFIG_DIR", home.join("htree"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env_remove("NOSTR_RELAYS")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            stderr: String::new(),
        }
    }

    fn until(&mut self, text: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|_| panic!("no {text:?} in stderr: {}", self.stderr));
            self.stderr.push_str(&line);
            self.stderr.push('\n');
            if line.contains(text) {
                return;
            }
        }
    }

    fn finish(&mut self) -> String {
        self.finish_with_status(true)
    }

    fn finish_with_status(&mut self, success: bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "installer did not exit");
            thread::sleep(Duration::from_millis(10));
        };
        for line in &self.lines {
            self.stderr.push_str(&format!("{line}\n"));
        }
        assert_eq!(status.success(), success, "{}", self.stderr);
        std::io::read_to_string(self.child.stdout.take().unwrap()).unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn install_reports_waits_and_verified_download_before_completion() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let payload = tmp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("app"), vec![42u8; 1024 * 1024])?;
    fs::write(payload.join("resource"), vec![43u8; 1024 * 1024])?;
    let author = nostr::Keys::generate();
    let repo_path = tmp.path().join("repo");
    let repo = Repository::local(repo_path.clone())?;
    let release = repo
        .publish(
            &author,
            PackageSpec {
                name: "hello".into(),
                version: "1.0.0".parse()?,
                target: target().into(),
                description: "Progress fixture".into(),
                commands: BTreeMap::new(),
                app: None,
                source: None,
                desktop: None,
            },
            &payload,
        )
        .await?;
    let manifest: Manifest = repo.json(&release.data.manifest).await?;
    let file_paths: Vec<_> = manifest
        .files
        .iter()
        .map(|file| {
            let cid = hashtree_core::Cid::parse(&file.cid).unwrap();
            format!(
                "/blobs/{}",
                haps::store::block_path(&cid.hash)
                    .to_string_lossy()
                    .replace('\\', "/")
            )
        })
        .collect();
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr());
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let (requested_tx, requested) = mpsc::channel();
    let (resume, paused) = mpsc::channel();
    let worker = thread::spawn(move || {
        while !stop_worker.load(Ordering::Relaxed) {
            let Some(request) = server.recv_timeout(Duration::from_millis(20)).unwrap() else {
                continue;
            };
            if request.url() == "/catalog.json"
                || file_paths.iter().any(|path| request.url() == path)
            {
                requested_tx.send(request.url().to_owned()).unwrap();
                if paused.recv_timeout(Duration::from_secs(15)).is_err() {
                    break;
                }
            }
            let file =
                fs::File::open(repo_path.join(request.url().trim_start_matches('/'))).unwrap();
            let _ = request.respond(tiny_http::Response::from_file(file));
        }
    });
    let home = tmp.path().join("reader");
    fs::create_dir(&home)?;
    fs::write(
        home.join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "identity": author.public_key().to_hex(),
            "sources": {"fixture": {"location": url, "author": author.public_key().to_hex(), "sequence": 0, "event_id": ""}},
            "discovery_defaults_version": 2
        }))?,
    )?;
    audit::record(&home, &author, release.event.id)?;
    let lock = haps::model::lock(&home.join(".cli.lock"))?;
    let mut cli = Running::start(&home, &["install", "hello"]);
    // Even another Haps process holding the home lock must not hide startup.
    cli.until("Preparing to install hello", Duration::from_secs(2));
    cli.until("Waiting for another Haps command", Duration::from_secs(2));
    drop(lock);
    assert_eq!(
        requested.recv_timeout(Duration::from_secs(5))?,
        "/catalog.json"
    );
    cli.until("Reading package catalogs", Duration::from_secs(2));
    // The remote is deliberately stalled: output must arrive before it replies.
    cli.until("elapsed", Duration::from_secs(7));
    assert!(cli.child.try_wait()?.is_none());
    resume.send(())?;
    requested.recv_timeout(Duration::from_secs(5))?;
    cli.until("Downloading and verifying", Duration::from_secs(2));
    cli.until("0 B / 2.0 MiB", Duration::from_secs(7));
    assert!(cli.child.try_wait()?.is_none());
    resume.send(())?;
    requested.recv_timeout(Duration::from_secs(5))?;
    cli.until("1.0 MiB / 2.0 MiB (50%); 1/2 files", Duration::from_secs(7));
    assert!(cli.child.try_wait()?.is_none());
    resume.send(())?;
    assert!(cli.finish().contains("Installed"));
    assert!(
        cli.stderr.contains("2.0 MiB / 2.0 MiB (100%)"),
        "{}",
        cli.stderr
    );
    assert!(cli.stderr.contains("2/2 files"));
    assert!(cli.stderr.contains("Registering package"));
    println!("{}", cli.stderr);

    // JSON stays a single machine-readable result, including an update with no changes.
    let mut json = Running::start(&home, &["update", "hello", "--json"]);
    requested.recv_timeout(Duration::from_secs(5))?;
    resume.send(())?;
    let result: serde_json::Value = serde_json::from_str(&json.finish())?;
    assert_eq!(result["status"], "installed");
    assert!(json.stderr.is_empty(), "{}", json.stderr);

    let mut current = Running::start(&home, &["update", "hello"]);
    current.until("Checking for updates to hello", Duration::from_secs(2));
    requested.recv_timeout(Duration::from_secs(5))?;
    resume.send(())?;
    assert!(current.finish().contains("Installed"));
    assert!(current.stderr.contains("Package is already current"));
    assert!(!current.stderr.contains("Downloading and verifying"));
    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    Ok(())
}

#[test]
fn failed_install_stops_progress_and_keeps_json_errors_clean() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut cli = Running::start(tmp.path(), &["install", "missing"]);
    cli.until("Preparing to install missing", Duration::from_secs(2));
    assert!(cli.finish_with_status(false).is_empty());
    assert!(
        cli.stderr
            .contains("haps: package discovery is unavailable")
    );
    assert!(!cli.stderr.contains("Registering package"));

    let mut json = Running::start(tmp.path(), &["install", "missing", "--json"]);
    let result: serde_json::Value = serde_json::from_str(&json.finish_with_status(false))?;
    assert_eq!(result["error"]["code"], "operation_failed");
    assert!(json.stderr.is_empty(), "{}", json.stderr);
    Ok(())
}
