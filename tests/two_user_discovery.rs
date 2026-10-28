use anyhow::{Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, atomic::AtomicBool},
};

#[path = "support/network.rs"]
mod network;
#[path = "support/tree.rs"]
mod tree;

struct User {
    home: PathBuf,
    helpers: PathBuf,
    relay: String,
}

impl User {
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_haps"));
        command
            .current_dir(&self.home)
            .env("HAPS_HOME", &self.home)
            .env("HAPS_NO_DEFAULTS", "true")
            .env("HAPS_NON_INTERACTIVE", "true")
            .env("HTREE_CONFIG_DIR", self.home.join("hashtree"))
            .env("HTREE_PREFER_LOCAL_DAEMON", "false")
            .env("HTREE_LOCAL_DAEMON_ONLY", "false")
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("NOSTR_RELAYS", &self.relay)
            .env("PATH", &self.helpers)
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        checked(self.command(args).output()?, args)
    }
}

fn checked(output: Output, args: &[&str]) -> Result<Output> {
    ensure!(
        output.status.success(),
        "haps {args:?}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn config(home: &Path) -> Result<serde_json::Value> {
    Ok(serde_json::from_slice(&fs::read(
        home.join("config.json"),
    )?)?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publisher_cli_to_fresh_second_user_search_install_and_run() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let blobs = Arc::new(hashtree_core::MemoryStore::new());
    let state = network::RelayState {
        events: Arc::new(tokio::sync::Mutex::new(vec![])),
        catalog: temp.path().join("unused-catalog"),
        accept: Arc::new(AtomicBool::new(true)),
    };
    let served_blobs = blobs.clone();
    let app = axum::Router::new()
        .route("/ws", axum::routing::get(network::relay))
        .route("/catalog/*path", axum::routing::get(network::catalog_file))
        .route(
            "/blocks/:hash",
            axum::routing::get(
                move |axum::extract::Path(hash): axum::extract::Path<String>| {
                    let blobs = served_blobs.clone();
                    async move {
                        use axum::response::IntoResponse;
                        use hashtree_core::Store;
                        let Ok(hash) = hashtree_core::from_hex(hash.trim_end_matches(".bin"))
                        else {
                            return axum::http::StatusCode::BAD_REQUEST.into_response();
                        };
                        match blobs.get(&hash).await.unwrap() {
                            Some(bytes) => bytes.into_response(),
                            None => axum::http::StatusCode::NOT_FOUND.into_response(),
                        }
                    }
                },
            ),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // Only the external htree upload executable is substituted. The fixture
    // stages real Hashtree blocks on the HTTP server below; all signing, relay
    // publication, discovery, verification, installation and execution use Haps.
    let helpers = temp.path().join("helpers");
    fs::create_dir(&helpers)?;
    let helper_source = temp.path().join("htree.rs");
    fs::write(
        &helper_source,
        r#"fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--version"] { println!("htree 0.2.151"); return; }
    assert_eq!(args[0], "add");
    assert!(std::path::Path::new(&args[1]).join("catalog.json").is_file());
    assert_eq!(&args[2..], ["--publish", "two-users"]);
    println!("permalink: https://drive.iris.to/#/{}", std::env::var("TEST_ROOT").unwrap());
}"#,
    )?;
    let helper = helpers.join(format!("htree{}", std::env::consts::EXE_SUFFIX));
    ensure!(
        Command::new("rustc")
            .arg(&helper_source)
            .arg("-o")
            .arg(&helper)
            .status()?
            .success()
    );

    let make_user = |name: &str| -> Result<User> {
        let home = temp.path().join(name);
        fs::create_dir_all(home.join("hashtree"))?;
        fs::write(
            home.join("hashtree/config.toml"),
            format!(
                "[storage]\ndata_dir={:?}\n[blossom]\nread_servers=[{:?}]\nservers=[]\n",
                home.join("hashtree/data"),
                url.clone() + "/blocks"
            ),
        )?;
        Ok(User {
            home,
            helpers: helpers.clone(),
            relay: url.replace("http:", "ws:") + "/ws",
        })
    };
    let publisher = make_user("publisher")?;
    let reader = make_user("reader")?;
    let publisher_id = String::from_utf8(publisher.run(&["identity", "init"])?.stdout)?
        .trim()
        .to_owned();
    let reader_id = String::from_utf8(reader.run(&["identity", "init"])?.stdout)?
        .trim()
        .to_owned();
    assert_ne!(publisher_id, reader_id);

    let payload = publisher.home.join("stage");
    fs::create_dir(&payload)?;
    let program = publisher.home.join("hello.rs");
    fs::write(
        &program,
        "fn main() { println!(\"Hello from another publisher\"); }",
    )?;
    let executable = format!("hello{}", std::env::consts::EXE_SUFFIX);
    ensure!(
        Command::new("rustc")
            .arg(&program)
            .arg("-o")
            .arg(payload.join(&executable))
            .status()?
            .success()
    );
    publisher.run(&[
        "init",
        "--name",
        "hello-two-users",
        "--version",
        "1.0.0",
        "--command",
        &executable,
        "--description",
        "Friendly greeting",
    ])?;
    let release: nostr::Event = serde_json::from_slice(
        &publisher
            .run(&[
                "pack",
                "haps.toml",
                "--payload",
                "stage",
                "--out",
                "catalog",
            ])?
            .stdout,
    )?;
    assert_eq!(release.pubkey.to_hex(), publisher_id);
    release.verify()?;
    let tree = hashtree_core::HashTree::new(hashtree_core::HashTreeConfig::new(blobs));
    let root = tree::add_directory(&tree, &publisher.home.join("catalog")).await?;
    let root = hashtree_core::nhash_encode_full(&hashtree_core::NHashData {
        hash: root.hash,
        decrypt_key: root.key,
    })?;
    let args = ["publish", "catalog", "--name", "two-users"];
    checked(
        publisher.command(&args).env("TEST_ROOT", root).output()?,
        &args,
    )?;
    assert_eq!(
        fs::read_dir(publisher.home.join("discovery/outbox"))?.count(),
        0
    );
    let events = state.events.lock().await.clone();
    assert_eq!(
        events.len(),
        2,
        "the CLI must publish both head and release"
    );
    assert!(events.contains(&release));
    assert!(
        events
            .iter()
            .all(|event| event.pubkey.to_hex() == publisher_id)
    );

    // The second user receives only the publisher's public identity. No source,
    // index, signed events, catalog path or cache is copied to their home.
    reader.run(&["follow", &publisher_id])?;
    let reader_config = config(&reader.home)?;
    assert!(reader_config["sources"].as_object().unwrap().is_empty());
    assert!(reader_config["indexes"].as_object().unwrap().is_empty());
    let cache = haps::discovery::Discovery::open(&reader.home)?;
    assert!(cache.events(vec![nostr::Filter::new()]).await?.is_empty());
    drop(cache);
    // Removing the publisher's catalog also rules out accidental local reads.
    fs::remove_dir_all(publisher.home.join("catalog"))?;
    let search = reader.run(&["search", "hello-two-users", "--json"])?;
    let found: serde_json::Value = serde_json::from_slice(&search.stdout)?;
    assert_eq!(
        found.as_array().unwrap().len(),
        1,
        "cold reader search: {} {}",
        String::from_utf8_lossy(&search.stderr),
        String::from_utf8_lossy(&search.stdout)
    );
    assert_eq!(found[0]["publisher"], publisher_id);
    assert_eq!(found[0]["package"]["name"], "hello-two-users");
    assert_eq!(found[0]["release_id"], release.id.to_hex());

    let package = format!("{publisher_id}/hello-two-users");
    let denied = reader.command(&["install", &package, "--json"]).output()?;
    assert!(
        !denied.status.success(),
        "discovery must not bypass the audit gate"
    );
    assert!(String::from_utf8_lossy(&denied.stdout).contains("audit"));
    reader.run(&["install", &package, "--allow-unaudited", "--json"])?;
    let installed = haps::install::Installation::new(reader.home.clone())?.with_desktop_dir(None);
    assert_eq!(installed.receipt("hello-two-users")?.current, release);
    assert_eq!(
        fs::read(installed.command("hello-two-users", None)?)?,
        fs::read(payload.join(executable))?
    );
    assert_eq!(
        String::from_utf8(reader.run(&["run", "hello-two-users"])?.stdout)?.trim(),
        "Hello from another publisher"
    );
    server.abort();
    Ok(())
}
