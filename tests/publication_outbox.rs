use haps::{
    discovery::{Announcement, Discovery},
    model::{PackageSpec, target},
};
use nostr::Keys;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Clone)]
struct Relay {
    rejected: Arc<tokio::sync::Mutex<std::collections::BTreeSet<nostr::EventId>>>,
    rejected_names: Arc<tokio::sync::Mutex<std::collections::BTreeSet<String>>>,
    received: Arc<tokio::sync::Mutex<Vec<nostr::Event>>>,
}

async fn relay(
    axum::extract::State(state): axum::extract::State<Relay>,
    socket: axum::extract::WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    socket.on_upgrade(move |mut socket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            let axum::extract::ws::Message::Text(text) = message else {
                continue;
            };
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            if value[0] != "EVENT" {
                continue;
            }
            let event: nostr::Event = serde_json::from_value(value[1].clone()).unwrap();
            event.verify().unwrap();
            let rejected_name = match haps::model::tag_value(&event, "d") {
                Ok(name) => state.rejected_names.lock().await.contains(name),
                Err(_) => false,
            };
            let accepted = !state.rejected.lock().await.contains(&event.id) && !rejected_name;
            eprintln!("Fixture relay acknowledgement: {} {accepted}", event.id);
            state.received.lock().await.push(event.clone());
            if socket
                .send(axum::extract::ws::Message::Text(
                    serde_json::json!(["OK", event.id, accepted, "fixture acknowledgement"])
                        .to_string(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    })
}

fn announcement(keys: &Keys, name: &str) -> anyhow::Result<nostr::Event> {
    Announcement::sign(
        keys,
        &PackageSpec {
            name: name.into(),
            version: "1.0.0".parse()?,
            target: target().into(),
            description: "Publication fixture".into(),
            commands: BTreeMap::from([(name.into(), name.into())]),
            source: None,
            app: None,
            desktop: None,
        },
        "https://example.com/catalog",
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publication_requires_only_current_events_and_preserves_unrelated_outbox()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let keys = Keys::generate();
    let old = announcement(&keys, "old")?;
    let current = announcement(&keys, "current")?;
    let state = Relay {
        rejected: Arc::new(tokio::sync::Mutex::new([old.id].into())),
        rejected_names: Default::default(),
        received: Arc::new(tokio::sync::Mutex::new(Vec::new())),
    };
    let app = axum::Router::new()
        .route("/", axum::routing::get(relay))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("ws://{}/", listener.local_addr()?);
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let bus = nostr_pubsub_relay::RelayEventBus::new([&url], Duration::from_secs(1)).await?;
    let discovery = Discovery::open(temp.path())?;
    discovery.queue(&[old.clone(), current.clone()])?;
    let old_path = temp
        .path()
        .join(format!("discovery/outbox/{}.json", old.id));
    let original = std::fs::read(&old_path)?;
    assert_eq!(
        discovery
            .flush_events(&bus, std::slice::from_ref(&current))
            .await?,
        0,
        "an unrelated historical failure cannot fail an acknowledged current release"
    );
    assert_eq!(std::fs::read(&old_path)?, original);
    assert_eq!(
        state.received.lock().await.as_slice(),
        std::slice::from_ref(&current)
    );
    assert!(discovery.flush_events(&bus, &[]).await.is_err());
    assert!(
        discovery
            .flush_events(&bus, std::slice::from_ref(&current))
            .await
            .is_err()
    );
    let current_path = temp
        .path()
        .join(format!("discovery/outbox/{}.json", current.id));
    std::fs::write(&current_path, serde_json::to_vec(&old)?)?;
    assert!(
        discovery
            .flush_events(&bus, std::slice::from_ref(&current))
            .await
            .is_err()
    );
    discovery.queue(std::slice::from_ref(&current))?;
    state.rejected.lock().await.insert(current.id);
    assert_eq!(
        discovery
            .flush_events(&bus, std::slice::from_ref(&current))
            .await?,
        1,
        "a rejected current event must remain pending"
    );
    assert_eq!(std::fs::read(&old_path)?, original);
    state.rejected.lock().await.clear();
    assert_eq!(discovery.flush(&bus).await?, 0);
    task.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_exits_unsuccessfully_while_announcements_are_rejected() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let event = announcement(&Keys::generate(), "rejected")?;
    let state = Relay {
        rejected: Arc::new(tokio::sync::Mutex::new([event.id].into())),
        rejected_names: Default::default(),
        received: Arc::new(tokio::sync::Mutex::new(Vec::new())),
    };
    let app = axum::Router::new()
        .route("/", axum::routing::get(relay))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("ws://{}/", listener.local_addr()?);
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Discovery::open(temp.path())?.queue(std::slice::from_ref(&event))?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_haps"))
        .env("NOSTR_RELAYS", &url)
        .env("HTREE_CONFIG_DIR", temp.path().join("htree"))
        .env("HTREE_PREFER_LOCAL_DAEMON", "false")
        .args(["--no-defaults", "--non-interactive", "--home"])
        .arg(temp.path())
        .arg("sync")
        .output()?;
    assert_eq!(*state.received.lock().await, [event]);
    assert!(
        !output.status.success(),
        "sync must report incomplete delivery through its exit status"
    );
    assert_eq!(
        std::fs::read_dir(temp.path().join("discovery/outbox"))?.count(),
        1
    );
    task.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_import_release_scopes_acknowledgements_and_preserves_history_on_retry()
-> anyhow::Result<()> {
    use std::{fs, process::Command};
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("author");
    fs::create_dir(&home)?;
    let keys = Keys::generate();
    fs::write(home.join("identity.key"), keys.secret_key().to_secret_hex())?;
    let payload = temp.path().join("old-payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("hello"), b"old original bytes")?;
    fs::write(payload.join("untouched"), b"unrelated original bytes")?;
    let catalog = home.join("catalogs/tools/packages");
    let repo = haps::repository::Repository::local(catalog.clone())?;
    let mut historical = Vec::new();
    for name in ["hello", "untouched"] {
        historical.push(
            repo.publish(
                &keys,
                PackageSpec {
                    name: name.into(),
                    version: "0.9.0".parse()?,
                    target: target().into(),
                    description: "Retained original release".into(),
                    commands: BTreeMap::from([(name.into(), name.into())]),
                    source: None,
                    app: None,
                    desktop: None,
                },
                &payload,
            )
            .await?,
        );
    }
    let untouched = announcement(&keys, "untouched")?;
    let old_events = [historical[0].event.clone(), untouched];
    Discovery::open(&home)?.queue(&old_events)?;
    let old_rows: Vec<_> = old_events
        .iter()
        .map(|event| {
            let path = home.join(format!("discovery/outbox/{}.json", event.id));
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    let state = Relay {
        rejected: Arc::new(tokio::sync::Mutex::new([historical[0].event.id].into())),
        rejected_names: Arc::new(tokio::sync::Mutex::new(["untouched".into()].into())),
        received: Default::default(),
    };
    let app = axum::Router::new()
        .route("/", axum::routing::get(relay))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("ws://{}/", listener.local_addr()?);
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let tools = temp.path().join("helpers");
    fs::create_dir(&tools)?;
    let helper_source = tools.join("helper.rs");
    fs::write(
        &helper_source,
        r#"
fn main() {
    let mut args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--version"] { println!("htree 0.2.150"); return; }
    if args.last().is_some_and(|arg| arg == "--help") { println!("Established helper, no optional acceleration"); return; }
    if args[0] == "--data-dir" { args.drain(..2); }
    if args[0] == "nostr-index" {
        std::fs::copy(&args[3], std::env::var_os("INDEX_EVENTS").unwrap()).unwrap();
        println!("{{\"root\":\"{}\"}}", std::env::var("TEST_ROOT").unwrap());
    } else if args[0] == "add" { println!("url: {}", std::env::var("TEST_ROOT").unwrap()); }
    else { assert_eq!(args[0], "push"); }
}
"#,
    )?;
    assert!(
        Command::new("rustc")
            .arg(&helper_source)
            .arg("-o")
            .arg(tools.join(format!("htree{}", std::env::consts::EXE_SUFFIX)))
            .status()?
            .success()
    );
    let release = temp.path().join("release");
    fs::create_dir(&release)?;
    let python = std::env::var_os("HAPS_PYTHON").unwrap_or_else(|| "python3".into());
    assert!(Command::new(python).arg("-c").arg(r#"
import hashlib,io,json,pathlib,sys,tarfile
root=pathlib.Path(sys.argv[1]); payload=b'new immutable executable'
archive=root/'hello-v1.0.0.tar.gz'
with tarfile.open(archive,'w:gz') as output:
    member=tarfile.TarInfo('hello');member.size=len(payload);member.mode=0o755
    output.addfile(member,io.BytesIO(payload))
(root/'release.json').write_text(json.dumps({'tag':'v1.0.0','commit':'a'*40,'draft':False,'assets':[{'name':archive.name,'path':archive.name,'size':archive.stat().st_size,'sha256':hashlib.sha256(archive.read_bytes()).hexdigest()}]}))
"#).arg(&release).status()?.success());
    let mapping = temp.path().join("mapping.json");
    fs::write(
        &mapping,
        serde_json::to_vec(&serde_json::json!({
            "publisher": keys.public_key().to_hex(), "catalog":"tools", "source":"https://example.com/tools",
            "packages":[{"name":"hello","description":"Current release","target":target(),"asset":"hello-{tag}.tar.gz","commands":{"hello":"hello"}}]
        }))?,
    )?;
    let events_file = temp.path().join("public-index-events.json");
    let mut path = vec![tools];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(path)?;
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_haps"))
            .env("PATH", &path)
            .env("NOSTR_RELAYS", &url)
            .env("HTREE_CONFIG_DIR", temp.path().join("htree"))
            .env("HTREE_PREFER_LOCAL_DAEMON", "false")
            .env("INDEX_EVENTS", &events_file)
            .env("TEST_ROOT", hashtree_core::nhash_encode(&[7; 32]).unwrap())
            .args(["--no-defaults", "--non-interactive", "--home"])
            .arg(&home)
            .arg("import-release")
            .arg(&release)
            .arg("--config")
            .arg(&mapping)
            .args(["--tag", "v1.0.0", "--publish"])
            .output()
    };
    let first = run()?;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_wire = state.received.lock().await.clone();
    assert_eq!(
        first_wire.len(),
        3,
        "only current release, imported package head, and index require ACKs"
    );
    let current = first_wire
        .iter()
        .find(|event| event.kind == haps::model::APP_KIND)
        .unwrap();
    assert_eq!(
        haps::model::Release::verify(current.clone())?
            .data
            .package
            .version
            .to_string(),
        "1.0.0"
    );
    let indexed: Vec<nostr::Event> = serde_json::from_slice(&fs::read(&events_file)?)?;
    for release in &historical {
        assert!(indexed.contains(&release.event));
    }
    let retained = repo
        .releases(&repo.catalog(&keys.public_key().to_hex()).await?)
        .await?;
    assert_eq!(retained.len(), 3);
    for release in &historical {
        assert!(retained.iter().any(|value| value.event == release.event));
    }
    state.rejected_names.lock().await.insert("hello".into());
    let failed = run()?;
    assert!(
        !failed.status.success(),
        "a rejected current package head must fail the import"
    );
    state.rejected_names.lock().await.remove("hello");
    let retry = run()?;
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let received = state.received.lock().await;
    assert_eq!(received.len(), 9);
    assert!(
        received[3..].iter().all(|event| first_wire.contains(event)),
        "unchanged retry must preserve exact release/head/index signatures"
    );
    for (path, bytes) in old_rows {
        assert_eq!(fs::read(path)?, bytes);
    }
    assert_eq!(Discovery::open(&home)?.pending()?, 2);
    task.abort();
    Ok(())
}
