use haps::{
    model::{PackageSpec, target},
    repository::Repository,
};
use std::{collections::BTreeMap, fs, process::Command};

#[path = "support/network.rs"]
mod network;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_op_and_failed_upload_retry_keep_server_scoped_successful_checkpoints()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("home");
    fs::create_dir(&home)?;
    let keys = nostr::Keys::generate();
    fs::write(home.join("identity.key"), keys.secret_key().to_secret_hex())?;
    fs::write(
        home.join("config.json"),
        serde_json::to_vec(
            &serde_json::json!({"identity":keys.public_key().to_hex(),"sources":{},"discovery_defaults_version":2}),
        )?,
    )?;
    let payload = temp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("hello"), b"first immutable release")?;
    let catalog = home.join("catalogs/default/packages");
    let repository = Repository::local(catalog.clone())?;
    let spec = |version: &str| PackageSpec {
        name: "hello".into(),
        version: version.parse().unwrap(),
        target: target().into(),
        description: "Upload checkpoint fixture".into(),
        commands: BTreeMap::from([("hello".into(), "hello".into())]),
        dependencies: Vec::new(),
        source: None,
        app: None,
        desktop: None,
    };
    let first = repository.publish(&keys, spec("1.0.0"), &payload).await?;
    haps::discovery::Discovery::open(&home.join("catalogs/default"))?
        .ingest([first.event])
        .await?;
    let state = network::RelayState {
        events: Default::default(),
        catalog,
        accept: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let app = axum::Router::new()
        .route("/", axum::routing::get(network::relay))
        .route("/catalog/*path", axum::routing::get(network::catalog_file))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let relay = format!("ws://{}/", listener.local_addr()?);
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let tools = temp.path().join("helpers");
    fs::create_dir(&tools)?;
    let source = tools.join("helper.rs");
    fs::write(
        &source,
        r#"
fn main() {
    use std::io::Write;
    let original: Vec<_> = std::env::args().skip(1).collect();
    let mut args = original.clone();
    if args == ["--version"] { println!("htree 0.2.155"); return; }
    if args.last().is_some_and(|arg| arg == "--help") { println!("--cache --previous-root"); return; }
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(std::env::var_os("HELPER_LOG").unwrap()).unwrap();
    writeln!(log, "{}", original.join("\t")).unwrap();
    if args[0] == "--data-dir" { args.drain(..2); }
    if args[0] == "add" {
        if cfg!(unix) {
            assert_eq!(&args[2..4], ["--local", "--cache"]);
            assert!(!std::path::Path::new(&args[4]).starts_with(&args[1]));
        } else { assert_eq!(&args[2..], ["--local"]); }
        println!("url: {}", std::env::var("TEST_ROOT").unwrap());
    } else if args[0] == "nostr-index" {
        println!("{{\"root\":\"{}\"}}", std::env::var("TEST_ROOT").unwrap());
    } else {
        assert_eq!(args[0], "push");
        assert!(!args[1].starts_with("htree://"));
        if std::env::var_os("PUSH_FAIL").is_some() { std::process::exit(1); }
    }
}
"#,
    )?;
    assert!(
        Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(tools.join(format!("htree{}", std::env::consts::EXE_SUFFIX)))
            .status()?
            .success()
    );
    let mut path = vec![tools];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(path)?;
    let log = temp.path().join("helper.log");
    let config = temp.path().join("htree");
    fs::create_dir(&config)?;
    fs::write(
        config.join("config.toml"),
        "[blossom]\nwrite_servers=['https://first.example']\n",
    )?;
    let root_a = hashtree_core::nhash_encode(&[1; 32])?;
    let root_b = hashtree_core::nhash_encode(&[2; 32])?;
    let run = |root: &str, fail: bool, data: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_haps"));
        command
            .current_dir(temp.path())
            .env("PATH", &path)
            .env("NOSTR_RELAYS", &relay)
            .env("HTREE_CONFIG_DIR", &config)
            .env("HTREE_DATA_DIR", data)
            .env("HTREE_PREFER_LOCAL_DAEMON", "false")
            .env("TEST_ROOT", root)
            .env("HELPER_LOG", &log)
            .args(["--no-defaults", "--non-interactive", "--home"])
            .arg(&home)
            .args(["catalog", "publish"]);
        if fail {
            command.env("PUSH_FAIL", "1");
        }
        command.output().unwrap()
    };
    let pushes = || -> anyhow::Result<Vec<Vec<String>>> {
        Ok(fs::read_to_string(&log)?
            .lines()
            .filter_map(|line| {
                let mut args: Vec<String> = line.split('\t').map(str::to_owned).collect();
                if args.first().is_some_and(|arg| arg == "--data-dir") {
                    args.drain(..2);
                }
                args.first()
                    .is_some_and(|arg| arg == "push")
                    .then_some(args)
            })
            .collect())
    };
    let published = run(&root_a, false, "first-data");
    assert!(
        published.status.success(),
        "{}",
        String::from_utf8_lossy(&published.stderr)
    );
    assert_eq!(pushes()?.len(), 2);
    assert!(run(&root_a, false, "first-data").status.success());
    assert_eq!(
        pushes()?.len(),
        2,
        "a warm unchanged retry must not upload catalog or event index again"
    );
    fs::write(payload.join("hello"), b"second immutable release")?;
    let second = repository.publish(&keys, spec("2.0.0"), &payload).await?;
    haps::discovery::Discovery::open(&home.join("catalogs/default"))?
        .ingest([second.event])
        .await?;
    let failed = run(&root_b, true, "first-data");
    assert!(!failed.status.success());
    assert_eq!(pushes()?[2], ["push", &root_b, "--previous-root", &root_a]);
    let retry = run(&root_b, false, "first-data");
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(
        pushes()?[3],
        ["push", &root_b, "--previous-root", &root_a],
        "failed upload must not advance the remembered previous root"
    );
    assert_eq!(pushes()?.len(), 5);
    assert!(run(&root_b, false, "first-data").status.success());
    assert_eq!(pushes()?.len(), 5);
    fs::write(
        config.join("config.toml"),
        "[blossom]\nwrite_servers=['https://second.example']\n",
    )?;
    assert!(run(&root_b, false, "first-data").status.success());
    assert_eq!(
        pushes()?[5],
        ["push", &root_b],
        "a new destination must receive a complete upload"
    );
    assert!(run(&root_b, false, "second-data").status.success());
    assert_eq!(
        pushes()?[7],
        ["push", &root_b],
        "a new store must not reuse the old upload checkpoint"
    );
    #[cfg(unix)]
    {
        fs::rename(home.join("publication"), home.join("retained-publication"))?;
        let public = home.join("catalogs/default/packages");
        let before = fs::read_dir(&public)?.count();
        std::os::unix::fs::symlink(&public, home.join("publication"))?;
        assert!(
            !run(&root_b, false, "second-data").status.success(),
            "a private cache symlink into the public catalog must be rejected"
        );
        assert_eq!(
            fs::read_dir(public)?.count(),
            before,
            "rejected private-state paths must not mutate the public catalog"
        );
    }
    task.abort();
    Ok(())
}
