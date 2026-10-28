#![cfg(unix)]
use haps::{
    model::{PackageSpec, SourceInfo, target},
    repository::Repository,
};
use nostr::nips::nip19::ToBech32;
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, process::Command};

fn executable(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    fs::write(path, text)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[tokio::test]
async fn publishing_pushes_pinned_source_and_blocks_announcement_on_cold_fetch_failure()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let source = root.join("source");
    fs::create_dir(&source)?;
    let real_git = String::from_utf8(Command::new("which").arg("git").output()?.stdout)?
        .trim()
        .to_owned();
    let git = |args: &[&str]| {
        let output = Command::new(&real_git)
            .current_dir(&source)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    fs::write(source.join("README"), "public source")?;
    git(&["add", "."]);
    git(&["commit", "-m", "source"]);
    let rev = git(&["rev-parse", "HEAD"]);
    let keys = nostr::Keys::generate();
    let home = root.join("home");
    fs::create_dir(&home)?;
    fs::write(home.join("identity.key"), keys.secret_key().to_secret_hex())?;
    let payload = root.join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("hello"), "binary")?;
    let catalog = root.join("catalog");
    Repository::local(catalog.clone())?
        .publish(
            &keys,
            PackageSpec {
                name: "hello".into(),
                version: "1.0.0".parse()?,
                target: target().into(),
                description: "fixture".into(),
                commands: BTreeMap::from([("hello".into(), "hello".into())]),
                dependencies: vec![],
                app: None,
                desktop: None,
                source: Some(SourceInfo {
                    git: format!("htree://{}/hello", keys.public_key().to_bech32()?),
                    rev: rev.clone(),
                }),
            },
            &payload,
        )
        .await?;
    let tools = root.join("tools");
    fs::create_dir(&tools)?;
    executable(
        &tools.join("git-remote-htree"),
        r#"#!/usr/bin/env python3
import json, os, sys
if sys.argv[1:] == ['--publisher-capabilities'] and not os.environ.get('OLD_HELPER'):
    if os.environ.get('BAD_CAPABILITIES'):
        print('{"schema": 1, "helper": "git-remote-htree", "capabilities": ["push"]}')
    else:
        print(json.dumps({'schema': 1, 'helper': 'git-remote-htree', 'version': '0.2.93',
                          'capabilities': ['complete-blob-dag-v1']}))
    sys.exit(0)
print('Usage: git-remote-htree', file=sys.stderr)
sys.exit(1)
"#,
    )?;
    executable(
        &tools.join("htree"),
        r#"#!/usr/bin/env python3
import os, sys
if sys.argv[1:] == ['--version']:
    print('htree 0.2.151')
    sys.exit(0)
with open(os.environ['MARKER'], 'a') as f: f.write('package-upload\n')
sys.exit(1)
"#,
    )?;
    executable(
        &tools.join("git"),
        r#"#!/usr/bin/env python3
import os, sys
args = sys.argv[1:]
if 'push' in args:
    assert args[-1] == os.environ['REV'] + ':refs/tags/haps-source/' + os.environ['REV']
    assert args[-2].startswith('htree://')
    with open(os.environ['MARKER'], 'a') as f: f.write('source-push\n')
    sys.exit(1 if os.environ.get('FAIL_PUSH') else 0)
if 'clone' in args:
    assert os.environ['HTREE_CONFIG_DIR'] != os.environ['PUBLISHER_CONFIG']
    assert os.environ['HTREE_PREFER_LOCAL_DAEMON'] == 'false'
    assert os.environ['HTREE_LOCAL_DAEMON_ONLY'] == 'false'
    assert not os.path.exists(os.environ['HTREE_DATA_DIR'])
    assert '--no-local' in args
    with open(os.environ['MARKER'], 'a') as f: f.write('cold-fetch\n')
    if os.environ.get('FAIL_FETCH'):
        print('pack unavailable; failed to traverse parents', file=sys.stderr)
        sys.exit(1)
    args = [os.environ['SOURCE_URL'] if a.startswith('htree://') else a for a in args]
os.execv(os.environ['REAL_GIT'], [os.environ['REAL_GIT']] + args)
"#,
    )?;
    let marker = root.join("marker");
    let mut paths = vec![tools];
    paths.extend(
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .filter(|directory| !directory.join("git-remote-htree").exists()),
    );
    let path = std::env::join_paths(paths)?;
    for (failure, expected) in [
        ("OLD_HELPER", ""),
        ("BAD_CAPABILITIES", ""),
        ("FAIL_PUSH", "source-push\n"),
        ("FAIL_FETCH", "source-push\ncold-fetch\n"),
        ("NONE", "source-push\ncold-fetch\npackage-upload\n"),
    ] {
        fs::write(&marker, "")?;
        let result = Command::new(env!("CARGO_BIN_EXE_haps"))
            .current_dir(&source)
            .env("PATH", &path)
            .env("REAL_GIT", &real_git)
            .env("MARKER", &marker)
            .env("REV", &rev)
            .env(
                "SOURCE_URL",
                reqwest::Url::from_directory_path(&source).unwrap().as_str(),
            )
            .env("HTREE_CONFIG_DIR", root.join("publisher-cache"))
            .env("PUBLISHER_CONFIG", root.join("publisher-cache"))
            .env("HAPS_NO_DEFAULTS", "true")
            .env(failure, "1")
            .arg("--home")
            .arg(&home)
            .args(["publish"])
            .arg(&catalog)
            .args(["--name", "fixture"])
            .output()?;
        assert!(!result.status.success()); // fake package uploader deliberately fails
        assert_eq!(
            fs::read_to_string(&marker)?,
            expected,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        if failure == "FAIL_FETCH" {
            assert!(
                String::from_utf8_lossy(&result.stderr).contains("public source is incomplete")
            );
        }
        if matches!(failure, "OLD_HELPER" | "BAD_CAPABILITIES") {
            assert!(String::from_utf8_lossy(&result.stderr).contains("updated Git helper"));
        }
    }
    Ok(())
}
