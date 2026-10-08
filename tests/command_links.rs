use haps::{
    install::Installation,
    model::{PackageSpec, target},
    repository::Repository,
};
use nostr::Keys;
use std::{collections::BTreeMap, fs, process::Command};

#[tokio::test]
async fn manager_launcher_tracks_update_rollback_and_removal() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().join("manager home");
    let payload = tmp.path().join("payload");
    fs::create_dir(&payload)?;
    let executable = format!("haps{}", std::env::consts::EXE_SUFFIX);
    fs::copy(env!("CARGO_BIN_EXE_haps"), payload.join(&executable))?;
    let keys = Keys::generate();
    let repo = Repository::local(tmp.path().join("catalog"))?;
    let spec = |version: &str| PackageSpec {
        name: "haps".into(),
        version: version.parse().unwrap(),
        target: target().into(),
        description: "Manager".into(),
        commands: BTreeMap::from([("haps".into(), executable.clone())]),
        source: None,
        app: None,
        desktop: None,
    };
    let first = repo.publish(&keys, spec("1.0.0"), &payload).await?;
    let second = repo.publish(&keys, spec("1.1.0"), &payload).await?;
    let installation = Installation::new(home.clone())?.with_desktop_dir(None);
    installation.install(&repo, &first).await?;
    let bin = installation.link("haps", None)?;
    let launcher = bin.join(if cfg!(windows) { "haps.cmd" } else { "haps" });
    let invoke = |args: &[&str]| -> anyhow::Result<String> {
        // Let Rust apply Windows batch-file quoting; cmd /C with ordinary
        // arguments strips the launcher's quotes when its path contains spaces.
        let mut command = Command::new(&launcher);
        let output = command
            .args(args)
            .env("HAPS_NO_DEFAULTS", "true")
            .env("HTREE_CONFIG_DIR", tmp.path().join("hashtree"))
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    let check = || -> anyhow::Result<()> {
        assert_eq!(
            invoke(&["path", "haps"])?,
            installation.path("haps")?.to_str().unwrap()
        );
        Ok(())
    };
    check()?;
    invoke(&["identity", "init"])?;
    invoke(&["follow", &keys.public_key().to_hex()])?;
    invoke(&[
        "source",
        "add",
        "tools",
        repo.path().unwrap().to_str().unwrap(),
        "--author",
        &keys.public_key().to_hex(),
    ])?;
    // Update through the running manager's launcher, including Windows .cmd.
    invoke(&["update", "haps"])?;
    assert_eq!(installation.receipt("haps")?.current.id, second.event.id);
    check()?;
    invoke(&["rollback", "haps"])?;
    assert_eq!(installation.receipt("haps")?.current.id, first.event.id);
    check()?;
    installation.remove("haps")?;
    assert!(!launcher.exists());
    Ok(())
}
