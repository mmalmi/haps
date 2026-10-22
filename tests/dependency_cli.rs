#[path = "support/audit.rs"]
mod audit;
use haps::{
    dependencies::Dependency,
    install::Installation,
    model::{PackageSpec, target},
    repository::Repository,
};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::{Command, Output},
};

fn cli(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_haps"))
        .arg("--home")
        .arg(home)
        .env("HAPS_NO_DEFAULTS", "true")
        .env("HTREE_CONFIG_DIR", home.join("hashtree"))
        .env_remove("NOSTR_RELAYS")
        .args(args)
        .output()
        .unwrap()
}
fn ok(home: &Path, args: &[&str]) -> String {
    let output = cli(home, args);
    assert!(
        output.status.success(),
        "{args:?}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test]
async fn cli_audits_the_transitive_closure_and_exposes_commands_without_global_links()
-> anyhow::Result<()> {
    let temp = tempfile::Builder::new()
        .prefix("haps dependencies '")
        .tempdir()?;
    let home = temp.path().join("reader");
    let payload = temp.path().join("payload");
    fs::create_dir(&payload)?;
    let source = temp.path().join("fixture.rs");
    fs::write(
        &source,
        r#"fn main() {
        if std::env::args().nth(1).as_deref() == Some("child") { println!("dependency executed"); return; }
        let output = std::process::Command::new("dependency-tool").arg("child").output().unwrap();
        assert!(output.status.success());
        print!("{}", String::from_utf8(output.stdout).unwrap());
    }"#,
    )?;
    let executable = format!("fixture{}", std::env::consts::EXE_SUFFIX);
    assert!(
        Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(payload.join(&executable))
            .status()?
            .success()
    );
    let repo = Repository::local(temp.path().join("catalog"))?;
    let keys = nostr::Keys::generate();
    let spec = |name: &str, command: Option<&str>, dependencies: Vec<Dependency>| PackageSpec {
        name: name.into(),
        version: "1.0.0".parse().unwrap(),
        target: target().into(),
        description: "Dependency CLI fixture".into(),
        commands: command
            .map(|name| BTreeMap::from([(name.into(), executable.clone())]))
            .unwrap_or_default(),
        dependencies,
        source: None,
        app: None,
        desktop: None,
    };
    let leaf = repo
        .publish(
            &keys,
            spec("tool", Some("dependency-tool"), vec![]),
            &payload,
        )
        .await?;
    let pin = |release: &haps::model::Release| Dependency {
        package: release.identity(),
        release: release.event.id.to_hex(),
    };
    let middle = repo
        .publish(&keys, spec("middle", None, vec![pin(&leaf)]), &payload)
        .await?;
    let root = repo
        .publish(
            &keys,
            spec("parent", Some("parent"), vec![pin(&middle)]),
            &payload,
        )
        .await?;
    ok(&home, &["identity", "init"]);
    ok(
        &home,
        &[
            "source",
            "add",
            "fixture",
            repo.path().unwrap().to_str().unwrap(),
            "--author",
            &keys.public_key().to_hex(),
        ],
    );
    audit::local(&home, root.event.id)?;
    audit::local(&home, middle.event.id)?;
    let denied = cli(
        &home,
        &["--non-interactive", "install", &root.identity(), "--json"],
    );
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stdout).contains("dependency"));
    let installed = Installation::new(home.clone())?.with_desktop_dir(None);
    assert!(
        installed.receipts()?.is_empty(),
        "an unaudited transitive dependency must block all activation"
    );
    audit::local(&home, leaf.event.id)?;
    ok(
        &home,
        &["--non-interactive", "install", &root.identity(), "--json"],
    );
    assert_eq!(installed.receipts()?.len(), 3);
    assert_eq!(ok(&home, &["run", "parent"]).trim(), "dependency executed");
    assert!(
        !home.join("bin").exists(),
        "dependencies must not claim global command names"
    );
    let bin = installed.link("parent", None)?;
    let launcher = bin.join(if cfg!(windows) {
        "parent.cmd"
    } else {
        "parent"
    });
    let output = Command::new(launcher).output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout)?.trim(),
        "dependency executed"
    );
    let mut next_leaf = leaf.data.package.clone();
    next_leaf.version = "2.0.0".parse()?;
    let leaf2 = repo.publish(&keys, next_leaf, &payload).await?;
    let mut next_middle = middle.data.package.clone();
    next_middle.version = "2.0.0".parse()?;
    next_middle.dependencies = vec![pin(&leaf2)];
    let middle2 = repo.publish(&keys, next_middle, &payload).await?;
    let mut next_root = root.data.package.clone();
    next_root.version = "2.0.0".parse()?;
    next_root.dependencies = vec![pin(&middle2)];
    let root2 = repo.publish(&keys, next_root, &payload).await?;
    for release in [&root2, &middle2, &leaf2] {
        audit::local(&home, release.event.id)?;
    }
    ok(&home, &["--non-interactive", "update", "parent", "--json"]);
    assert_eq!(installed.receipt("tool")?.current.id, leaf2.event.id);
    assert_eq!(ok(&home, &["run", "parent"]).trim(), "dependency executed");
    ok(&home, &["rollback", "parent"]);
    assert_eq!(installed.receipt("parent")?.current.id, root.event.id);
    assert_eq!(installed.receipt("middle")?.current.id, middle.event.id);
    assert_eq!(installed.receipt("tool")?.current.id, leaf.event.id);
    assert_eq!(ok(&home, &["run", "parent"]).trim(), "dependency executed");
    assert!(!cli(&home, &["remove", "tool"]).status.success());
    ok(&home, &["remove", "parent"]);
    ok(&home, &["remove", "middle"]);
    ok(&home, &["remove", "tool"]);
    Ok(())
}

#[tokio::test]
async fn explicit_parent_does_not_trust_foreign_dependencies_or_override_mutes()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("reader");
    let payload = temp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("data"), "fixture")?;
    let repo = Repository::local(temp.path().join("repo"))?;
    let foreign_repo = Repository::local(temp.path().join("foreign-repo"))?;
    let parent = nostr::Keys::generate();
    let foreign = nostr::Keys::generate();
    let mut spec = PackageSpec {
        name: "foreign".into(),
        version: "1.0.0".parse()?,
        target: target().into(),
        description: "Trust fixture".into(),
        commands: BTreeMap::new(),
        dependencies: vec![],
        source: None,
        app: None,
        desktop: None,
    };
    let dependency = foreign_repo
        .publish(&foreign, spec.clone(), &payload)
        .await?;
    spec.name = "parent".into();
    spec.dependencies = vec![Dependency {
        package: dependency.identity(),
        release: dependency.event.id.to_hex(),
    }];
    let root = repo.publish(&parent, spec, &payload).await?;
    ok(&home, &["identity", "init"]);
    for (name, key, source) in [
        ("parent", &parent, &repo),
        ("foreign", &foreign, &foreign_repo),
    ] {
        ok(
            &home,
            &[
                "source",
                "add",
                name,
                source.path().unwrap().to_str().unwrap(),
                "--author",
                &key.public_key().to_hex(),
            ],
        );
    }
    let denied = cli(
        &home,
        &[
            "--non-interactive",
            "install",
            &root.identity(),
            "--allow-unaudited",
            "--json",
        ],
    );
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stdout).contains("not authorized"));
    let installed = Installation::new(home.clone())?.with_desktop_dir(None);
    assert!(installed.receipts()?.is_empty());
    ok(
        &home,
        &[
            "--non-interactive",
            "install",
            &root.identity(),
            "--allow-unaudited",
            "--allow-untrusted",
            "--json",
        ],
    );
    assert_eq!(installed.receipts()?.len(), 2);
    let me = haps::install::load_keys(&home.join("identity.key"))?;
    let mute = nostr::EventBuilder::new(nostr::Kind::MuteList, "")
        .tags([nostr::Tag::public_key(foreign.public_key())])
        .sign_with_keys(&me)?;
    let import = temp.path().join("mute.json");
    fs::write(&import, serde_json::to_vec(&mute)?)?;
    ok(&home, &["import", import.to_str().unwrap()]);
    let muted = cli(
        &home,
        &[
            "--non-interactive",
            "install",
            &root.identity(),
            "--allow-unaudited",
            "--allow-untrusted",
            "--json",
        ],
    );
    assert!(!muted.status.success());
    assert!(String::from_utf8_lossy(&muted.stdout).contains("not authorized"));
    assert_eq!(installed.receipt("parent")?.current.id, root.event.id);
    Ok(())
}
