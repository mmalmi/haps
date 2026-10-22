#![cfg(unix)]
use haps::{
    bootstrap::Installer,
    install::Installation,
    model::{PackageSpec, target},
    repository::Repository,
};
use std::{collections::BTreeMap, fs, os::unix::fs::symlink, path::Path, process::Command};

fn bundle(bin: &Path) -> anyhow::Result<std::path::PathBuf> {
    let executable = bin.join(".haps/v0.1.11.fixture/haps");
    fs::create_dir_all(executable.parent().unwrap())?;
    fs::copy(env!("CARGO_BIN_EXE_haps"), &executable)?;
    symlink(".haps/v0.1.11.fixture/haps", bin.join("haps"))?;
    Ok(executable)
}

#[test]
fn cargo_update_explains_the_command_without_initializing_or_building() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let cargo = tmp.path().join("cargo home");
    let bin = cargo.join("bin/haps");
    fs::create_dir_all(bin.parent().unwrap())?;
    fs::copy(env!("CARGO_BIN_EXE_haps"), &bin)?;
    let home = tmp.path().join("haps home");
    let output = Command::new(&bin)
        .args(["update", "haps", "--json", "--no-defaults"])
        .env("CARGO_HOME", &cargo)
        .env("HAPS_HOME", &home)
        .env("HTREE_CONFIG_DIR", tmp.path().join("htree"))
        .output()?;
    assert!(!output.status.success());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cargo install haps --locked"),
        "{result}"
    );
    assert!(!home.exists());
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn recognizes_only_the_active_installer_link() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let bin = tmp.path().join("bin with spaces");
    let executable = bundle(&bin)?;
    let installer = Installer::from_executable(&executable)?.unwrap();
    assert_eq!(installer.directory(), bin.canonicalize()?);
    fs::remove_file(bin.join("haps"))?;
    fs::write(bin.join("haps"), "unrelated command")?;
    assert!(Installer::from_executable(&executable)?.is_none());
    assert!(installer.verify().is_err());
    fs::remove_file(bin.join("haps"))?;
    symlink(&executable, bin.join("haps"))?;
    assert!(Installer::from_executable(&executable)?.is_none());
    Ok(())
}

#[tokio::test]
async fn adopted_command_survives_update_and_rollback_and_preserves_bootstrap() -> anyhow::Result<()>
{
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().join("manager home");
    let bin = tmp.path().join("bin with spaces");
    let original = bundle(&bin)?;
    let installer = Installer::from_executable(&original)?.unwrap();
    let payload = tmp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::copy(&original, payload.join("haps"))?;
    let keys = nostr::Keys::generate();
    let repo = Repository::local(tmp.path().join("catalog"))?;
    let spec = |version: &str| PackageSpec {
        name: "haps".into(),
        version: version.parse().unwrap(),
        target: target().into(),
        description: "Bootstrap fixture".into(),
        commands: BTreeMap::from([("haps".into(), "haps".into())]),
        dependencies: Vec::new(),
        source: None,
        app: None,
        desktop: None,
    };
    let first = repo.publish(&keys, spec("1.0.0"), &payload).await?;
    let second = repo.publish(&keys, spec("1.1.0"), &payload).await?;
    let installation = Installation::new(home.clone())?.with_desktop_dir(None);
    installation.install_with_policy(&repo, &first, 2).await?;
    installation.adopt_installer(&first.identity(), &installer)?;
    assert!(!fs::symlink_metadata(bin.join("haps"))?.is_symlink());
    assert!(original.is_file());
    let invoke = |args: &[&str]| -> anyhow::Result<String> {
        let output = Command::new(bin.join("haps"))
            .args(args)
            .env("HAPS_NO_DEFAULTS", "true")
            .env("HTREE_CONFIG_DIR", tmp.path().join("htree"))
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    assert_eq!(
        invoke(&["path", "haps"])?,
        installation.path("haps")?.to_str().unwrap()
    );
    installation.install_with_policy(&repo, &second, 1).await?;
    assert_eq!(installation.receipt("haps")?.minimum_attestations, 2);
    assert_eq!(
        invoke(&["path", "haps"])?,
        installation.path("haps")?.to_str().unwrap()
    );
    installation.rollback("haps")?;
    assert_eq!(installation.receipt("haps")?.current.id, first.event.id);
    assert_eq!(
        invoke(&["path", "haps"])?,
        installation.path("haps")?.to_str().unwrap()
    );
    assert!(original.is_file());
    Ok(())
}

#[tokio::test]
async fn bootstrap_never_selects_another_publishers_haps() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().join("manager");
    let bin = tmp.path().join("bin");
    let original = bundle(&bin)?;
    let repo = Repository::local(tmp.path().join("catalog"))?;
    let keys = nostr::Keys::generate();
    let payload = tmp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("haps"), "unrelated publisher")?;
    repo.publish(
        &keys,
        PackageSpec {
            name: "haps".into(),
            version: "99.0.0".parse()?,
            target: target().into(),
            description: "Unrelated publisher".into(),
            commands: BTreeMap::from([("haps".into(), "haps".into())]),
            dependencies: Vec::new(),
            source: None,
            app: None,
            desktop: None,
        },
        &payload,
    )
    .await?;
    fs::create_dir(&home)?;
    fs::write(
        home.join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "identity": null, "discovery_defaults_version": 2,
            "sources": {"unrelated": {"location": repo.path().unwrap(), "author": keys.public_key().to_hex(), "sequence": 0, "event_id": ""}}
        }))?,
    )?;
    let output = Command::new(bin.join("haps"))
        .args([
            "update",
            "haps",
            "--json",
            "--no-defaults",
            "--allow-unaudited",
        ])
        .env("HAPS_HOME", &home)
        .env("HTREE_CONFIG_DIR", tmp.path().join("htree"))
        .env_remove("NOSTR_RELAYS")
        .output()?;
    assert!(!output.status.success());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no matching release"),
        "{result}"
    );
    assert!(!home.join("installed.json").exists());
    assert!(Installer::from_executable(&original)?.is_some());
    Ok(())
}

#[tokio::test]
async fn bundled_helpers_follow_updates_without_claiming_foreign_commands() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for foreign_htree in [false, true] {
        let tmp = tempfile::tempdir()?;
        let bin = tmp.path().join("bin");
        let original = bundle(&bin)?;
        let old_helpers = original.parent().unwrap().join("libexec");
        fs::create_dir(&old_helpers)?;
        for helper in ["htree", "git-remote-htree"] {
            fs::write(old_helpers.join(helper), "old helper")?;
            symlink(
                format!(".haps/v0.1.11.fixture/libexec/{helper}"),
                bin.join(helper),
            )?;
        }
        if foreign_htree {
            fs::remove_file(bin.join("htree"))?;
            fs::write(bin.join("htree"), "#!/bin/sh\necho foreign\n")?;
            fs::set_permissions(bin.join("htree"), fs::Permissions::from_mode(0o755))?;
        }
        let installer = Installer::from_executable(&original)?.unwrap();
        let payload = tmp.path().join("payload");
        fs::create_dir_all(payload.join("libexec"))?;
        fs::write(payload.join("haps"), "fixture manager")?;
        let repo = Repository::local(tmp.path().join("repo"))?;
        let keys = nostr::Keys::generate();
        let installation = Installation::new(tmp.path().join("home"))?.with_desktop_dir(None);
        for (version, answer) in [("1.0.0", "first"), ("1.1.0", "second")] {
            for helper in ["htree", "git-remote-htree"] {
                fs::write(
                    payload.join("libexec").join(helper),
                    format!("#!/bin/sh\necho {answer}\n"),
                )?;
            }
            let release = repo
                .publish(
                    &keys,
                    PackageSpec {
                        name: "haps".into(),
                        version: version.parse()?,
                        target: target().into(),
                        description: "Bundled helpers".into(),
                        commands: BTreeMap::from([
                            ("haps".into(), "haps".into()),
                            ("htree".into(), "libexec/htree".into()),
                            ("git-remote-htree".into(), "libexec/git-remote-htree".into()),
                        ]),
                        dependencies: Vec::new(),
                        source: None,
                        app: None,
                        desktop: None,
                    },
                    &payload,
                )
                .await?;
            installation.install(&repo, &release).await?;
            if answer == "first" {
                installation.adopt_installer(&release.identity(), &installer)?;
            }
            for helper in ["htree", "git-remote-htree"] {
                let output = Command::new(bin.join(helper)).output()?;
                assert!(output.status.success());
                let expected = if foreign_htree && helper == "htree" {
                    "foreign"
                } else {
                    answer
                };
                assert_eq!(String::from_utf8(output.stdout)?.trim(), expected);
            }
        }
        installation.rollback("haps")?;
        assert_eq!(
            String::from_utf8(Command::new(bin.join("git-remote-htree")).output()?.stdout)?.trim(),
            "first"
        );
        installation.remove("haps")?;
        assert!(!bin.join("git-remote-htree").exists());
        assert_eq!(bin.join("htree").exists(), foreign_htree);
        assert!(old_helpers.join("htree").exists());
    }
    Ok(())
}
