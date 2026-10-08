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
        .env("HTREE_CONFIG_DIR", home.join("hashtree"))
        .env("HAPS_NO_DEFAULTS", "true")
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
        "fn main() { let arg = std::env::args().nth(1).unwrap_or_default(); if arg == \"resources\" { println!(\"{}\", std::env::var(\"XDG_DATA_DIRS\").unwrap()); } else { println!(\"hello {}\", arg); } }",
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
    let initialized: serde_json::Value = serde_json::from_str(&ok(
        &author_home,
        &[
            "init",
            "--name",
            "hello",
            "--version",
            "1.0.0",
            "--description",
            "Friendly greeting tool",
            "--command",
            &format!("bin/{executable}"),
            "--payload",
            payload.to_str().unwrap(),
            "--out",
            manifest.to_str().unwrap(),
            "--json",
        ],
    ))
    .unwrap();
    assert_eq!(initialized["package"]["target"], haps::model::target());
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
    ok(
        &reader_home,
        &[
            "attest",
            &release.id.to_hex(),
            "--audited",
            "--provenance",
            "Test fixture",
            "--note",
            "Reviewed greeting fixture",
        ],
    );
    ok(&reader_home, &["install", &format!("{publisher}/hello")]);
    #[cfg(target_os = "linux")]
    {
        let output = Command::new(env!("CARGO_BIN_EXE_haps"))
            .env("HAPS_NO_DEFAULTS", "true")
            .env("HTREE_CONFIG_DIR", reader_home.join("hashtree"))
            .env("XDG_DATA_DIRS", "/custom resources:/usr/share")
            .arg("--home")
            .arg(&reader_home)
            .args(["run", "hello", "--", "resources"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let directory = ok(&reader_home, &["path", "hello"]);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!("{directory}/usr/share:{directory}/share:/custom resources:/usr/share")
        );
    }
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
        } else {
            args.extend(["--audited", "--provenance", "Test fixture"]);
        }
        ok(&reader_home, &args);
    };
    review(release.id, false);
    ok(
        &reader_home,
        &[
            "install",
            &format!("{publisher}/hello"),
            "--require-attestations",
            "1",
        ],
    );
    review(release.id, true);
    assert!(
        !cli(
            &reader_home,
            &[
                "install",
                &format!("{publisher}/hello"),
                "--allow-untrusted"
            ]
        )
        .status
        .success()
    );
    // Immediate re-approval must supersede revocation even within the same second.
    review(release.id, false);
    ok(&reader_home, &["install", &format!("{publisher}/hello")]);
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
        !cli(
            &reader_home,
            &[
                "install",
                &format!("{publisher}/hello"),
                "--allow-untrusted"
            ]
        )
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
fn init_is_offline_noninteractive_and_preserves_existing_files() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("unused-home");
    let payload = temp.path().join("stage");
    fs::create_dir(&payload).unwrap();
    fs::write(payload.join("hello"), b"staged binary").unwrap();
    let manifest = temp.path().join("haps.toml");
    let run = |command: &str, output: &Path| {
        cli(
            &home,
            &[
                "--non-interactive",
                "init",
                "--name",
                "hello",
                "--command",
                command,
                "--payload",
                payload.to_str().unwrap(),
                "--out",
                output.to_str().unwrap(),
                "--json",
            ],
        )
    };
    for (entry, output) in [
        ("../outside", manifest.clone()),
        ("missing", manifest.clone()),
        ("hello", payload.join("haps.toml")),
    ] {
        let result = run(entry, &output);
        assert!(!result.status.success());
        let error: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(error["error"]["code"], "operation_failed");
        assert!(!output.exists());
    }
    assert!(run("hello", &manifest).status.success());
    let original = fs::read(&manifest).unwrap();
    assert!(!run("hello", &manifest).status.success());
    assert_eq!(fs::read(&manifest).unwrap(), original);
    assert!(
        !home.exists(),
        "init must not create identity or configuration"
    );
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
    let mut releases: Vec<nostr::Event> = Vec::new();
    for name in ["alice", "bob"] {
        let home = tmp.path().join(name);
        let key = ok(&home, &["identity", "init"]);
        let repo = tmp.path().join(format!("{name}-repo"));
        let packed = ok(
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
        ok(&reader, &["alias", "add", name, &key]);
        releases.push(serde_json::from_str(&packed).unwrap());
        authors.push(key);
    }
    let shared_aliases = reader.join("hashtree/aliases");
    assert!(
        fs::read_to_string(shared_aliases)
            .unwrap()
            .contains(" alice")
    );
    assert!(
        !cli(&reader, &["alias", "add", "alice", &authors[1]])
            .status
            .success()
    );
    // Bare names exclude unknown publishers; explicit opt-in still ranks direct follows first.
    let keys =
        nostr::Keys::parse(&fs::read_to_string(reader.join("identity.key")).unwrap()).unwrap();
    let follows = nostr::EventBuilder::new(nostr::Kind::ContactList, "")
        .tags([nostr::Tag::public_key(
            nostr::PublicKey::parse(&authors[1]).unwrap(),
        )])
        .custom_created_at(nostr::Timestamp::from(
            nostr::Timestamp::now().as_secs() + 10,
        ))
        .sign_with_keys(&keys)
        .unwrap();
    let follow_file = tmp.path().join("follows.json");
    fs::write(&follow_file, serde_json::to_vec(&follows).unwrap()).unwrap();
    ok(&reader, &["import", follow_file.to_str().unwrap()]);
    ok(
        &reader,
        &[
            "attest",
            &releases[1].id.to_hex(),
            "--audited",
            "--provenance",
            "Test fixture",
            "--note",
            "Reviewed Bob fixture",
        ],
    );
    ok(&reader, &["install", &format!("{}/same", authors[1])]);
    assert!(ok(&reader, &["list"]).contains(&authors[1]));
    let result = cli(&reader, &["install", "same", "--allow-untrusted"]);
    assert!(!result.status.success());
    let choices = String::from_utf8_lossy(&result.stderr);
    assert!(choices.contains("multiple publishers"));
    assert!(choices.find("bob/same").unwrap() < choices.find("alice/same").unwrap());
    ok(&reader, &["remove", "bob/same"]);
    ok(
        &reader,
        &[
            "attest",
            &releases[0].id.to_hex(),
            "--audited",
            "--provenance",
            "Test fixture",
            "--note",
            "Reviewed Alice fixture",
        ],
    );
    ok(&reader, &["install", "alice/same"]);
    ok(&reader, &["update", "alice/same", "--allow-untrusted"]);
    use nostr::nips::nip19::ToBech32;
    let npub = nostr::PublicKey::parse(&authors[0])
        .unwrap()
        .to_bech32()
        .unwrap();
    assert!(ok(&reader, &["info", &format!("{npub}/same")]).contains(&authors[0]));
    let list = ok(&reader, &["list"]);
    assert!(list.contains(&authors[0]));
    assert!(!list.contains(&authors[1]));
}

#[test]
fn fresh_install_has_a_visible_replaceable_maintainer_starting_point() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_haps"))
            .env("HTREE_CONFIG_DIR", tmp.path().join("hashtree"))
            .env_remove("HAPS_NO_DEFAULTS")
            .arg("--home")
            .arg(&home)
            .args(args)
            .output()
            .unwrap()
    };
    let first = run(&["starting-point"]);
    assert!(first.status.success());
    assert!(String::from_utf8_lossy(&first.stderr).contains("Sirius Business Ltd"));
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join("config.json")).unwrap()).unwrap();
    assert!(config["sources"].as_object().unwrap().is_empty());
    assert!(
        config["indexes"]["haps"]["location"]
            .as_str()
            .unwrap()
            .starts_with("htree://nhash")
    );
    let root: nostr::Event =
        serde_json::from_str(include_str!("../packages/catalog-root.json")).unwrap();
    root.verify().unwrap();
    assert_eq!(config["indexes"]["haps"]["author"], root.pubkey.to_hex());
    assert_eq!(config["indexes"]["haps"]["event_id"], root.id.to_hex());
    assert_eq!(config["discovery_defaults_version"], 2);
    // Existing network profiles migrate once; later explicit changes survive.
    let mut legacy = config;
    legacy
        .as_object_mut()
        .unwrap()
        .remove("discovery_defaults_version");
    legacy["indexes"] = serde_json::json!({});
    fs::write(
        home.join("config.json"),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    assert!(run(&["starting-point"]).status.success());
    let mut migrated: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join("config.json")).unwrap()).unwrap();
    assert!(migrated["indexes"]["haps"].is_object());
    // Upgrade the exact retired preset and mutable default source together.
    migrated["discovery_defaults_version"] = 1.into();
    migrated["indexes"]["haps"]["location"] =
        "htree://npub1q6g6t3yk0m2ppp5mrqsze4xg6uqhw5p29kjutet5m3uk637xjfaqac3p2a/package-index"
            .into();
    migrated["indexes"]["haps"]["author"] =
        "731fd6f74667cac0e86b7b4f7cd2c828996db866c3044368a2f26d87cb571ad0".into();
    migrated["sources"]["iris"] = serde_json::json!({
        "location": format!("htree://{}/haps-packages", hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB),
        "author": root.pubkey.to_hex(), "sequence": 6, "event_id": "previous"
    });
    migrated["sources"]["custom"] = serde_json::json!({
        "location": "https://example.test/catalog", "author": root.pubkey.to_hex(),
        "sequence": 3, "event_id": "custom-pin"
    });
    fs::write(
        home.join("config.json"),
        serde_json::to_vec(&migrated).unwrap(),
    )
    .unwrap();
    assert!(run(&["starting-point"]).status.success());
    migrated = serde_json::from_slice(&fs::read(home.join("config.json")).unwrap()).unwrap();
    assert_eq!(migrated["discovery_defaults_version"], 2);
    assert_eq!(migrated["indexes"]["haps"]["event_id"], root.id.to_hex());
    assert!(migrated["sources"].get("iris").is_none());
    assert_eq!(migrated["sources"]["custom"]["event_id"], "custom-pin");
    migrated["indexes"] = serde_json::json!({});
    fs::write(
        home.join("config.json"),
        serde_json::to_vec(&migrated).unwrap(),
    )
    .unwrap();
    assert!(run(&["starting-point"]).status.success());
    let preserved: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join("config.json")).unwrap()).unwrap();
    assert!(preserved["indexes"].as_object().unwrap().is_empty());
    let help = String::from_utf8(run(&["--help"]).stdout).unwrap();
    assert!(!help.contains("  source "));
    assert!(!help.contains("  index "));
    assert!(run(&["starting-point", "--clear"]).status.success());
    assert_eq!(
        String::from_utf8_lossy(&run(&["starting-point"]).stdout).trim(),
        "none"
    );
}

#[test]
fn attestation_shortcut_pins_version_platform_and_exports_signed_claims() {
    let tmp = tempdir().unwrap();
    let publisher = tmp.path().join("publisher");
    let reader = tmp.path().join("reader");
    let author = ok(&publisher, &["identity", "init"]);
    let signer = ok(&reader, &["identity", "init"]);
    let payload = tmp.path().join("payload");
    fs::create_dir(&payload).unwrap();
    fs::write(payload.join("data"), b"attestation fixture").unwrap();
    let spec = tmp.path().join("haps.toml");
    let repo = tmp.path().join("repo");
    let host = haps::model::target();
    let other = if host == "x86_64-pc-windows-msvc" {
        "aarch64-apple-darwin"
    } else {
        "x86_64-pc-windows-msvc"
    };
    let mut releases = Vec::new();
    for (version, target) in [("1.0.0", host), ("1.0.0", other), ("2.0.0", host)] {
        fs::write(&spec, format!("name=\"hello\"\nversion=\"{version}\"\ntarget=\"{target}\"\ndescription=\"Fixture\"\n")).unwrap();
        let event: nostr::Event = serde_json::from_str(&ok(
            &publisher,
            &[
                "pack",
                spec.to_str().unwrap(),
                "--payload",
                payload.to_str().unwrap(),
                "--out",
                repo.to_str().unwrap(),
            ],
        ))
        .unwrap();
        releases.push(event);
    }
    ok(
        &reader,
        &[
            "source",
            "add",
            "alice",
            repo.to_str().unwrap(),
            "--author",
            &author,
        ],
    );
    ok(&reader, &["alias", "add", "alice", &author]);
    ok(&reader, &["alias", "add", "me", &signer]);
    // Never infer the latest release when signing a claim.
    for args in [
        vec!["attest", "alice/hello", "--note", "Checked", "--json"],
        vec![
            "attest",
            "alice/hello",
            "--version",
            "9.0.0",
            "--note",
            "Checked",
            "--json",
        ],
        vec![
            "attest",
            "alice/hello",
            "--version",
            "1.0.0",
            "--note",
            " ",
            "--json",
        ],
    ] {
        let result = cli(&reader, &args);
        assert!(!result.status.success());
        let error: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(error["error"]["code"], "operation_failed");
    }
    let note = "Ran tests\n\u{1b}[2Jnot terminal commands";
    let signed: nostr::Event = serde_json::from_str(&ok(
        &reader,
        &[
            "attest",
            "alice/hello",
            "--version",
            "1.0.0",
            "--note",
            note,
            "--json",
        ],
    ))
    .unwrap();
    signed.verify().unwrap();
    let claim = haps::trust::parse_attestation(&signed).unwrap();
    assert_eq!(signed.kind.as_u16(), 37368);
    assert!(signed.content.is_empty());
    assert_eq!(
        nostr_identity::parse_fact_snapshot_event(&signed)
            .unwrap()
            .subject,
        releases[0].id.to_hex()
    );
    assert_eq!(claim.release, releases[0].id.to_hex());
    assert_eq!(claim.note, note);
    assert!(claim.approved);
    assert!(
        reader
            .join("attestations")
            .join(format!("{}.json", signed.id))
            .exists()
    );
    let info: serde_json::Value = serde_json::from_str(&ok(
        &reader,
        &["info", "alice/hello", "--version", "1.0.0", "--json"],
    ))
    .unwrap();
    assert_eq!(info["attestations"][0]["label"], "me");
    assert_eq!(info["attestations"][0]["event"]["id"], signed.id.to_hex());
    // Agents can issue a release warning without prompts. It removes this
    // signer's endorsement and is visible in machine-readable discovery.
    let warning: nostr::Event = serde_json::from_str(&ok(
        &reader,
        &[
            "warn",
            "alice/hello",
            "--version",
            "1.0.0",
            "--note",
            "Unexpected outbound connection",
            "--json",
        ],
    ))
    .unwrap();
    let warning_claim = haps::trust::parse_attestation(&warning).unwrap();
    assert!(warning_claim.warning);
    assert!(!warning_claim.approved);
    assert_eq!(warning_claim.release, releases[0].id.to_hex());
    let warned_info: serde_json::Value = serde_json::from_str(&ok(
        &reader,
        &["info", "alice/hello", "--version", "1.0.0", "--json"],
    ))
    .unwrap();
    assert_eq!(warned_info["warnings"][0]["label"], "me");
    assert_eq!(
        warned_info["warnings"][0]["event"]["id"],
        warning.id.to_hex()
    );
    assert_eq!(warned_info["attestations"], serde_json::json!([]));
    let blocked = cli(
        &reader,
        &[
            "install",
            "alice/hello",
            "--version",
            "1.0.0",
            "--allow-untrusted",
            "--json",
        ],
    );
    assert!(!blocked.status.success());
    let error: serde_json::Value = serde_json::from_slice(&blocked.stdout).unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("trusted release warnings")
    );
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unexpected outbound connection")
    );
    assert!(!reader.join("installed.json").exists());
    // A warning override never bypasses the required audit.
    assert!(
        !cli(
            &reader,
            &[
                "install",
                "alice/hello",
                "--version",
                "1.0.0",
                "--allow-untrusted",
                "--allow-warnings",
                "--json"
            ]
        )
        .status
        .success()
    );
    assert!(
        !cli(
            &reader,
            &[
                "install",
                "alice/hello",
                "--version",
                "1.0.0",
                "--allow-untrusted",
                "--json"
            ]
        )
        .status
        .success()
    );
    ok(
        &reader,
        &[
            "warn",
            &releases[0].id.to_hex(),
            "--revoke",
            "--note",
            "Finding withdrawn",
            "--json",
        ],
    );
    let withdrawn: serde_json::Value = serde_json::from_str(&ok(
        &reader,
        &["info", "alice/hello", "--version", "1.0.0", "--json"],
    ))
    .unwrap();
    assert_eq!(withdrawn["warnings"], serde_json::json!([]));
    assert_eq!(withdrawn["attestations"], serde_json::json!([]));
    ok(
        &reader,
        &[
            "attest",
            &releases[0].id.to_hex(),
            "--audited",
            "--provenance",
            "Test fixture",
            "--note",
            note,
        ],
    );
    ok(&reader, &["install", "alice/hello", "--version", "1.0.0"]);
    // Updates enforce warnings before changing the active installation.
    ok(
        &reader,
        &[
            "warn",
            &releases[2].id.to_hex(),
            "--note",
            "New release regression",
            "--json",
        ],
    );
    let blocked_update = cli(
        &reader,
        &["update", "alice/hello", "--allow-untrusted", "--json"],
    );
    assert!(!blocked_update.status.success());
    let error: serde_json::Value = serde_json::from_slice(&blocked_update.stdout).unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("trusted release warnings")
    );
    let installed: serde_json::Value =
        serde_json::from_str(&ok(&reader, &["list", "--json"])).unwrap();
    assert_eq!(installed[0]["release_id"], releases[0].id.to_hex());
    ok(
        &reader,
        &[
            "warn",
            &releases[2].id.to_hex(),
            "--revoke",
            "--note",
            "Withdrawing update warning",
            "--json",
        ],
    );
    // Replayed warnings cannot restore a withdrawn claim.
    let stale_warning = tmp.path().join("stale-warning.json");
    fs::write(&stale_warning, serde_json::to_vec(&warning).unwrap()).unwrap();
    ok(&reader, &["import", stale_warning.to_str().unwrap()]);
    let withdrawn: serde_json::Value = serde_json::from_str(&ok(
        &reader,
        &["info", "alice/hello", "--version", "1.0.0", "--json"],
    ))
    .unwrap();
    assert_eq!(withdrawn["warnings"], serde_json::json!([]));
    ok(
        &reader,
        &[
            "attest",
            &releases[0].id.to_hex(),
            "--audited",
            "--provenance",
            "Test fixture",
            "--note",
            note,
            "--json",
        ],
    );
    // A package/version shortcut still binds one platform, never every build.
    let foreign: nostr::Event = serde_json::from_str(&ok(
        &reader,
        &[
            "attest",
            "alice/hello",
            "--version",
            "1.0.0",
            "--target",
            other,
            "--note",
            "Inspected foreign build",
            "--json",
        ],
    ))
    .unwrap();
    assert_eq!(
        haps::trust::parse_attestation(&foreign).unwrap().release,
        releases[1].id.to_hex()
    );
    let install = cli(
        &reader,
        &[
            "install",
            "alice/hello",
            "--version",
            "1.0.0",
            "--require-attestations",
            "1",
        ],
    );
    assert!(install.status.success());
    let output = String::from_utf8(install.stderr).unwrap();
    assert!(output.contains("Attested by me"));
    assert!(output.contains("\\n\\u{1b}[2J"));
    assert!(!output.contains('\u{1b}'));
    // A new release does not inherit claims about an old one.
    let failed = cli(&reader, &["update", "alice/hello", "--json"]);
    assert!(!failed.status.success());
    let error: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("found 0")
    );
    // Revocation supersedes the claim, including same-second commands.
    ok(
        &reader,
        &[
            "attest",
            "alice/hello",
            "--version",
            "1.0.0",
            "--revoke",
            "--note",
            "Found a bug",
            "--json",
        ],
    );
    let info: serde_json::Value = serde_json::from_str(&ok(
        &reader,
        &["info", "alice/hello", "--version", "1.0.0", "--json"],
    ))
    .unwrap();
    assert_eq!(info["attestations"], serde_json::json!([]));
    assert!(
        !cli(
            &reader,
            &[
                "install",
                "alice/hello",
                "--version",
                "1.0.0",
                "--allow-untrusted",
                "--json"
            ]
        )
        .status
        .success()
    );
    let search: serde_json::Value =
        serde_json::from_str(&ok(&reader, &["search", "hello", "--json"])).unwrap();
    // Only the foreign build still has a current vouch from the reader.
    assert_eq!(search.as_array().unwrap().len(), 1);
    assert_eq!(search[0]["release_id"], releases[1].id.to_hex());
    let list: serde_json::Value = serde_json::from_str(&ok(&reader, &["list", "--json"])).unwrap();
    assert_eq!(list[0]["release_id"], releases[0].id.to_hex());
}

#[test]
fn rapid_findings_advance_the_relay_replacement_timestamp() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("reader");
    ok(&home, &["identity", "init"]);
    let keys = nostr::Keys::parse(
        fs::read_to_string(home.join("identity.key"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    let release = nostr::EventId::from_hex(&"ab".repeat(32)).unwrap();
    // A slightly future local claim makes this deterministic across second boundaries.
    let previous = haps::trust::attest_at(
        &keys,
        release,
        true,
        "Earlier finding".into(),
        (nostr::Timestamp::now().as_secs() + 2) * 1000,
    )
    .unwrap();
    let file = temp.path().join("previous.json");
    fs::write(&file, serde_json::to_vec(&previous).unwrap()).unwrap();
    ok(&home, &["import", file.to_str().unwrap()]);
    let warning: nostr::Event = serde_json::from_str(&ok(
        &home,
        &[
            "warn",
            &release.to_hex(),
            "--note",
            "Changed finding",
            "--json",
        ],
    ))
    .unwrap();
    assert!(warning.created_at > previous.created_at);
    let withdrawn: nostr::Event = serde_json::from_str(&ok(
        &home,
        &[
            "warn",
            &release.to_hex(),
            "--revoke",
            "--note",
            "Withdrawn",
            "--json",
        ],
    ))
    .unwrap();
    assert!(withdrawn.created_at > warning.created_at);
}
