use haps::{
    model::{PackageSpec, target},
    repository::Repository,
};
use nostr::nips::nip19::ToBech32;
use std::{collections::BTreeMap, fs, process::Command};

#[tokio::test]
async fn publish_checks_catalog_before_using_existing_or_bundled_htree() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let catalog = temp.path().join("catalog with spaces");
    let payload = temp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("hello"), "hello")?;
    let keys = nostr::Keys::generate();
    let key_file = temp.path().join("publisher.key");
    fs::write(&key_file, keys.secret_key().to_secret_hex())?;
    Repository::local(catalog.clone())?
        .publish(
            &keys,
            PackageSpec {
                name: "hello".into(),
                version: "1.0.0".parse()?,
                target: target().into(),
                description: "Publication fixture".into(),
                commands: BTreeMap::from([("hello".into(), "hello".into())]),
                source: None,
                app: None,
                desktop: None,
            },
            &payload,
        )
        .await?;
    let bundle = temp.path().join("bundle");
    fs::create_dir_all(bundle.join("libexec"))?;
    let source = temp.path().join("htree.rs");
    fs::write(
        &source,
        r#"
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--version"] { println!("htree 0.2.151"); return; }
    assert_eq!(args[0], "add");
    assert!(std::path::Path::new(&args[1]).join("catalog.json").exists());
    assert_eq!(&args[2..], ["--publish", "my-packages"]);
    std::fs::write(std::env::var_os("PUBLISH_MARKER").unwrap(), std::env::current_exe().unwrap().to_string_lossy().as_bytes()).unwrap();
    if std::env::var_os("PUBLISH_FAIL").is_some() { std::process::exit(1); }
    println!("  published: {}/my-packages", std::env::var("TEST_HOST").unwrap());
}
"#,
    )?;
    let suffix = std::env::consts::EXE_SUFFIX;
    let helper = bundle.join("libexec").join(format!("htree{suffix}"));
    assert!(
        Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&helper)
            .status()?
            .success()
    );
    let haps = bundle.join(format!("haps{suffix}"));
    fs::copy(env!("CARGO_BIN_EXE_haps"), &haps)?;
    let marker = temp.path().join("marker");
    let run = |binary: &std::path::Path, path: &std::path::Path, fail: bool| {
        let mut command = Command::new(binary);
        command
            .env("PATH", path)
            .env("PUBLISH_MARKER", &marker)
            .env("TEST_HOST", keys.public_key().to_bech32().unwrap())
            .env("NOSTR_RELAYS", "")
            .env("HTREE_PREFER_LOCAL_DAEMON", "false")
            .env("HTREE_CONFIG_DIR", temp.path().join("hashtree"))
            .args(["--no-defaults", "--home"])
            .arg(temp.path().join("home"))
            .arg("publish")
            .arg(&catalog)
            .args(["--name", "my-packages", "--key-file"])
            .arg(&key_file);
        if fail {
            command.env("PUBLISH_FAIL", "1");
        }
        command.output().unwrap()
    };
    let empty = temp.path().join("empty");
    fs::create_dir(&empty)?;
    let bundled = run(&haps, &empty, false);
    assert!(
        bundled.status.success(),
        "{}",
        String::from_utf8_lossy(&bundled.stderr)
    );
    assert!(fs::read_to_string(&marker)?.contains("libexec"));
    assert_eq!(
        fs::read_dir(temp.path().join("home/discovery/outbox"))?.count(),
        2
    );
    let announcements = haps::discovery::Discovery::open(&temp.path().join("home"))?
        .announcements(Some(keys.public_key()))
        .await?;
    assert_eq!(announcements[0].name, "hello");
    let existing = temp.path().join("existing");
    fs::create_dir(&existing)?;
    fs::copy(&helper, existing.join(format!("htree{suffix}")))?;
    assert!(run(&haps, &existing, false).status.success());
    assert!(fs::read_to_string(&marker)?.contains("existing"));
    assert!(!run(&haps, &existing, true).status.success());
    let bare = temp.path().join(format!("haps{suffix}"));
    fs::copy(&haps, &bare)?;
    let missing = run(&bare, &empty, false);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("cargo install hashtree-cli"));
    fs::remove_file(&marker)?;
    let file = catalog.join("catalog.json");
    let mut event: serde_json::Value = serde_json::from_slice(&fs::read(&file)?)?;
    event["sig"] = "0".repeat(128).into();
    fs::write(file, serde_json::to_vec(&event)?)?;
    assert!(!run(&haps, &existing, false).status.success());
    assert!(!marker.exists(), "invalid catalogs must not be published");
    Ok(())
}
