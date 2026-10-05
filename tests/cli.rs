use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tempfile::tempdir;

fn cli(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_haps"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap()
}
fn ok(home: &Path, args: &[&str]) -> String {
    let output = cli(home, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

struct Server {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(root: PathBuf) -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let worker = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                if let Some(request) = server.recv_timeout(Duration::from_millis(20)).unwrap() {
                    let path = request.url().trim_start_matches('/');
                    if let Ok(path) = haps::model::safe_path(path)
                        && let Ok(file) = fs::File::open(root.join(path))
                    {
                        let _ = request.respond(tiny_http::Response::from_file(file));
                        continue;
                    }
                    let _ = request.respond(tiny_http::Response::empty(404));
                }
            }
        });
        Self {
            url,
            stop,
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

#[test]
fn real_cli_http_install_execute_update_comments_and_rollback() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let author_home = root.join("publisher");
    let reader_home = root.join("reader");
    let publisher = ok(&author_home, &["identity", "init"]);
    ok(&reader_home, &["identity", "init"]);
    let payload = root.join("payload");
    fs::create_dir_all(payload.join("bin")).unwrap();
    let fixture = root.join("hello.rs");
    fs::write(
        &fixture,
        "fn main() { println!(\"hello {}\", std::env::args().nth(1).unwrap_or_default()); }",
    )
    .unwrap();
    let executable = format!("hello{}", std::env::consts::EXE_SUFFIX);
    assert!(
        Command::new("rustc")
            .arg(&fixture)
            .arg("-o")
            .arg(payload.join("bin").join(&executable))
            .status()
            .unwrap()
            .success()
    );
    let manifest = root.join("haps.toml");
    let repository = root.join("repository");
    let write_manifest = |version: &str| {
        fs::write(&manifest, format!("name = \"hello\"\nversion = \"{version}\"\ntarget = \"{}\"\ndescription = \"Friendly greeting tool\"\n[commands]\nhello = \"bin/{executable}\"\n", haps::model::target())).unwrap();
    };
    write_manifest("1.0.0");
    let packed = ok(
        &author_home,
        &[
            "pack",
            manifest.to_str().unwrap(),
            "--payload",
            payload.to_str().unwrap(),
            "--out",
            repository.to_str().unwrap(),
        ],
    );
    let release: nostr::Event = serde_json::from_str(&packed).unwrap();
    let old_catalog = fs::read(repository.join("catalog.json")).unwrap();
    let server = Server::new(repository.clone());
    assert!(
        !cli(
            &reader_home,
            &[
                "source",
                "add",
                "bad",
                &server.url,
                "--author",
                &nostr::Keys::generate().public_key().to_hex()
            ]
        )
        .status
        .success()
    );
    ok(
        &reader_home,
        &["source", "add", "demo", &server.url, "--author", &publisher],
    );
    assert!(!cli(&reader_home, &["install", "hello"]).status.success());
    ok(&reader_home, &["follow", &publisher]);
    assert!(ok(&reader_home, &["search", "greeting"]).contains("\"follow_distance\": 1"));
    ok(&reader_home, &["install", "hello"]);
    assert_eq!(
        ok(&reader_home, &["run", "hello", "--", "world with spaces"]),
        "hello world with spaces"
    );
    let comment = root.join("comment.json");
    let first_comment = ok(
        &reader_home,
        &[
            "comment",
            "hello",
            "Works well on my machine",
            "--out",
            comment.to_str().unwrap(),
        ],
    );
    let event: nostr::Event = serde_json::from_slice(&fs::read(&comment).unwrap()).unwrap();
    assert_eq!(event.kind, nostr::Kind::Comment);
    assert!(ok(&reader_home, &["comments", "hello"]).contains("Works well"));
    // Another identity imports and replies to the same package thread.
    ok(
        &author_home,
        &[
            "source",
            "add",
            "self",
            repository.to_str().unwrap(),
            "--author",
            &publisher,
        ],
    );
    ok(&author_home, &["import", comment.to_str().unwrap()]);
    let reply = root.join("reply.json");
    ok(
        &author_home,
        &[
            "comment",
            "hello",
            "Thanks for testing",
            "--reply-to",
            &first_comment,
            "--out",
            reply.to_str().unwrap(),
        ],
    );
    ok(&reader_home, &["import", reply.to_str().unwrap()]);
    assert!(ok(&reader_home, &["comments", "hello"]).contains("Thanks for testing"));
    let release_comment = root.join("release-comment.json");
    ok(
        &reader_home,
        &[
            "comment",
            "hello",
            "Review of version one",
            "--release",
            &release.id.to_hex(),
            "--out",
            release_comment.to_str().unwrap(),
        ],
    );
    assert!(!ok(&reader_home, &["comments", "hello"]).contains("Review of version one"));
    assert!(
        ok(
            &reader_home,
            &["comments", "hello", "--release", &release.id.to_hex()]
        )
        .contains("Review of version one")
    );
    // Strengthening policy on an already installed release must persist.
    let review_path = root.join("review.json");
    let review = |id: nostr::EventId, revoke: bool| {
        let id = id.to_hex();
        let mut args = vec![
            "attest",
            &id,
            "--note",
            "Integration test review",
            "--out",
            review_path.to_str().unwrap(),
        ];
        if revoke {
            args.push("--revoke");
        }
        ok(&reader_home, &args);
    };
    review(release.id, false);
    ok(
        &reader_home,
        &["install", "hello", "--require-attestations", "1"],
    );
    review(release.id, true);
    assert!(
        !cli(&reader_home, &["install", "hello", "--allow-untrusted"])
            .status
            .success()
    );
    // Immediate re-approval must supersede revocation even within the same second.
    review(release.id, false);
    ok(&reader_home, &["install", "hello"]);
    write_manifest("1.1.0");
    let second = ok(
        &author_home,
        &[
            "pack",
            manifest.to_str().unwrap(),
            "--payload",
            payload.to_str().unwrap(),
            "--out",
            repository.to_str().unwrap(),
        ],
    );
    let second: nostr::Event = serde_json::from_str(&second).unwrap();
    assert!(
        !cli(&reader_home, &["update", "hello", "--allow-untrusted"])
            .status
            .success()
    );
    assert!(
        !cli(&reader_home, &["install", "hello", "--allow-untrusted"])
            .status
            .success()
    );
    review(second.id, false);
    ok(&reader_home, &["update", "hello"]);
    assert!(ok(&reader_home, &["list"]).contains("1.1.0"));
    ok(&reader_home, &["rollback", "hello"]);
    assert!(ok(&reader_home, &["list"]).contains("1.0.0"));
    // A mirror replaying an older signed catalog must not roll back discovery.
    fs::write(repository.join("catalog.json"), old_catalog).unwrap();
    let replay = cli(&reader_home, &["search", "hello"]);
    assert!(!replay.status.success());
    assert!(String::from_utf8_lossy(&replay.stderr).contains("rollback"));
    // Failed discovery leaves the working installation intact.
    assert_eq!(
        ok(&reader_home, &["run", "hello", "--", "still works"]),
        "hello still works"
    );
    ok(&reader_home, &["remove", "hello"]);
    assert!(ok(&reader_home, &["list"]).is_empty());
}

#[test]
fn duplicate_names_require_a_publisher_and_updates_keep_that_publisher() {
    let tmp = tempdir().unwrap();
    let reader = tmp.path().join("reader");
    ok(&reader, &["identity", "init"]);
    let payload = tmp.path().join("payload");
    fs::create_dir(&payload).unwrap();
    fs::write(payload.join("data"), b"safe data").unwrap();
    let spec = tmp.path().join("haps.toml");
    fs::write(&spec, format!("name = \"same\"\nversion = \"1.0.0\"\ntarget = \"{}\"\ndescription = \"Collision test\"\n", haps::model::target())).unwrap();
    let mut authors = Vec::new();
    for name in ["alice", "bob"] {
        let home = tmp.path().join(name);
        let key = ok(&home, &["identity", "init"]);
        let repo = tmp.path().join(format!("{name}-repo"));
        ok(
            &home,
            &[
                "pack",
                spec.to_str().unwrap(),
                "--payload",
                payload.to_str().unwrap(),
                "--out",
                repo.to_str().unwrap(),
            ],
        );
        ok(
            &reader,
            &[
                "source",
                "add",
                name,
                repo.to_str().unwrap(),
                "--author",
                &key,
            ],
        );
        ok(&reader, &["follow", &key]);
        authors.push(key);
    }
    let result = cli(&reader, &["install", "same"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("multiple publishers"));
    ok(&reader, &["install", &format!("{}/same", authors[0])]);
    ok(&reader, &["update", "same"]);
    let list = ok(&reader, &["list"]);
    assert!(list.contains(&authors[0]));
    assert!(!list.contains(&authors[1]));
}
