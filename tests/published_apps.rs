//! Real public-catalog acceptance test. CI explicitly opts into network access.
use haps::{install::Installation, model::Release};
use std::{fs, process::Command};

#[test]
#[ignore = "downloads signed application payloads from the public Hashtree catalog"]
fn published_example_apps_install() -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(
            haps::model::target(),
            "aarch64-apple-darwin" | "x86_64-unknown-linux-gnu"
        ),
        "the example catalog does not yet cover this target"
    );
    anyhow::ensure!(
        std::env::var_os("CI").is_some()
            || std::env::var("HAPS_TEST_ISOLATED").as_deref() == Ok("1"),
        "run this test inside a VM/container with HAPS_TEST_ISOLATED=1"
    );
    let apps = std::env::var("HAPS_TEST_APPS").unwrap_or_else(|_| "iris-chat".into());
    for app in apps.split(',') {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("haps");
        let data = temp.path().join("data");
        anyhow::ensure!(
            matches!(app, "iris-chat" | "iris-drive" | "nostr-vpn"),
            "unknown example app"
        );
        let output = Command::new(env!("CARGO_BIN_EXE_haps"))
            .env("HAPS_HOME", &home)
            .env("XDG_DATA_HOME", &data)
            .env("HTREE_CONFIG_DIR", temp.path().join("hashtree"))
            .env("HTREE_PREFER_LOCAL_DAEMON", "false")
            .env_remove("HTREE_LOCAL_DAEMON_ONLY")
            .env_remove("HAPS_NO_DEFAULTS")
            .args(["--non-interactive", "install", app, "--json"])
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "{app} install failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout)?;
        let installed = Installation::new(home.clone())?.with_desktop_dir(None);
        let release = Release::verify(installed.receipt(app)?.current)?;
        assert_eq!(release.data.package.name, app);
        assert_eq!(release.data.package.target, haps::model::target());
        let executable = installed.command(app, Some(app))?;
        assert!(
            fs::metadata(&executable)?.len() > 1024,
            "missing real executable"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(fs::metadata(&executable)?.permissions().mode() & 0o111, 0);
        }
        #[cfg(target_os = "macos")]
        {
            let bundle = installed
                .path(app)?
                .join(release.data.package.app.as_ref().expect("macOS app bundle"));
            assert!(bundle.join("Contents/Info.plist").is_file());
            let check = Command::new("codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(bundle)
                .output()?;
            anyhow::ensure!(
                check.status.success(),
                "{app} signature failed: {}",
                String::from_utf8_lossy(&check.stderr)
            );
        }
        #[cfg(target_os = "linux")]
        {
            let launcher = data
                .join("applications")
                .join(haps::desktop::filename(&home.canonicalize()?, &release));
            let text = fs::read_to_string(launcher)?;
            assert!(
                text.contains(&executable.to_string_lossy().replace(' ', "\\ "))
                    || text.contains(executable.to_str().unwrap())
            );
        }
        eprintln!(
            "Installed and verified {app} {} ({})",
            release.data.package.version, release.event.id
        );
    }
    Ok(())
}
