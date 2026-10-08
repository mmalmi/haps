#[path = "support/audit.rs"]
mod audit;
use std::{fs, path::Path, process::Command};
use tempfile::tempdir;

fn git(root: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().into()
}

#[test]
fn pinned_source_preview_build_and_install() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("source");
    let home = tmp.path().join("home");
    fs::create_dir(&root).unwrap();
    git(&root, &["init"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "user.email", "test@example.invalid"]);
    fs::write(
        root.join("hello.rs"),
        "fn main() { println!(\"Built from pinned source\"); }",
    )
    .unwrap();
    fs::write(
        root.join("haps-build.toml"),
        r#"
[package]
name = "hello"
version = "1.0.0"
target = "host"
description = "Source build fixture"
[package.commands]
hello = "bin/hello{exe}"
[build]
commands = [["rustc", "hello.rs", "-o", "hello{exe}"]]
[build.artifacts]
"bin/hello{exe}" = "hello{exe}"
"#,
    )
    .unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "Source fixture"]);
    let rev = git(&root, &["rev-parse", "HEAD"]);
    let url = reqwest::Url::from_directory_path(&root)
        .unwrap()
        .to_string();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_haps"))
            .env("HTREE_CONFIG_DIR", home.join("hashtree"))
            .env("HAPS_NO_DEFAULTS", "true")
            .arg("--home")
            .arg(&home)
            .args(args)
            .output()
            .unwrap()
    };
    let preview = run(&["build", &url, "--rev", &rev]);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert!(String::from_utf8_lossy(&preview.stdout).contains("Preview only"));
    assert!(!home.join("built-packages").exists());
    assert!(!run(&["build", &url, "--rev", "master"]).status.success());
    assert!(run(&["identity", "init"]).status.success());
    assert!(
        !run(&["build", &url, "--rev", &rev, "--execute", "--install"])
            .status
            .success()
    );
    assert!(!home.join("built-packages").exists());
    let build = run(&["build", &url, "--rev", &rev, "--execute"]);
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let repo = haps::repository::Repository::local(home.join("built-packages")).unwrap();
    let keys = haps::install::load_keys(&home.join("identity.key")).unwrap();
    let snapshot = runtime
        .block_on(repo.catalog(&keys.public_key().to_hex()))
        .unwrap();
    let releases = runtime.block_on(repo.releases(&snapshot)).unwrap();
    audit::record(&home, &keys, releases[0].event.id).unwrap();
    assert!(
        run(&[
            "source",
            "add",
            "built",
            home.join("built-packages").to_str().unwrap(),
            "--author",
            &keys.public_key().to_hex()
        ])
        .status
        .success()
    );
    assert!(run(&["install", "hello"]).status.success());
    let executed = run(&["run", "hello"]);
    assert!(executed.status.success());
    assert_eq!(
        String::from_utf8_lossy(&executed.stdout).trim(),
        "Built from pinned source"
    );
    let info = fs::read_to_string(home.join("installed.json")).unwrap();
    assert!(info.contains(&rev));
}

#[test]
fn htree_build_uses_existing_or_bundled_helper_and_explains_missing_tool() {
    let tmp = tempdir().unwrap();
    let source = tmp.path().join("source");
    fs::create_dir(&source).unwrap();
    git(&source, &["init"]);
    git(&source, &["config", "user.name", "Test"]);
    git(&source, &["config", "user.email", "test@example.invalid"]);
    fs::write(
        source.join("haps-build.toml"),
        r#"
[package]
name = "fixture"
version = "1.0.0"
target = "host"
description = "Helper transport fixture"
[package.commands]
fixture = "bin/fixture{exe}"
[build]
commands = [["rustc", "fixture.rs"]]
[build.artifacts]
"bin/fixture{exe}" = "fixture{exe}"
"#,
    )
    .unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "Fixture"]);
    let rev = git(&source, &["rev-parse", "HEAD"]);
    let helper_source = tmp.path().join("helper.rs");
    fs::write(&helper_source, r#"
use std::{io::{self, BufRead, Write}, process::{Command, Stdio}};
fn main() {
    if std::env::args().len() == 1 {
        eprintln!("Usage: git-remote-htree <remote-name> <url>");
        std::process::exit(1);
    }
    std::fs::write(std::env::var_os("HELPER_MARKER").unwrap(), std::env::current_exe().unwrap().to_string_lossy().as_bytes()).unwrap();
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        match line.unwrap().as_str() {
            "capabilities" => { println!("connect\n"); io::stdout().flush().unwrap(); }
            "connect git-upload-pack" => {
                println!(); io::stdout().flush().unwrap();
                let status = Command::new("git").arg("upload-pack").arg(std::env::var_os("HELPER_REPO").unwrap())
                    .stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).status().unwrap();
                std::process::exit(status.code().unwrap_or(1));
            }
            "" => break,
            value => panic!("Unexpected Git helper request: {}", value),
        }
    }
}
"#).unwrap();
    let bundle = tmp.path().join("bundle with spaces");
    fs::create_dir_all(bundle.join("libexec")).unwrap();
    let suffix = std::env::consts::EXE_SUFFIX;
    let helper = bundle
        .join("libexec")
        .join(format!("git-remote-htree{suffix}"));
    assert!(
        Command::new("rustc")
            .arg(&helper_source)
            .arg("-o")
            .arg(&helper)
            .status()
            .unwrap()
            .success()
    );
    let haps = bundle.join(format!("haps{suffix}"));
    fs::copy(env!("CARGO_BIN_EXE_haps"), &haps).unwrap();
    let marker = tmp.path().join("marker");
    let url = "htree://npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/fixture";
    let run = |binary: &Path, path: &std::ffi::OsStr| {
        Command::new(binary)
            .env("PATH", path)
            .env("HELPER_REPO", &source)
            .env("HELPER_MARKER", &marker)
            .env("HTREE_CONFIG_DIR", tmp.path().join("hashtree"))
            .args(["--no-defaults", "--home"])
            .arg(tmp.path().join("home"))
            .args(["build", url, "--rev", &rev])
            .output()
            .unwrap()
    };
    // Only keep directories containing Git/system utilities, excluding any real
    // Hashtree helper on the developer's PATH.
    let paths: Vec<_> = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .filter(|p| !p.join(format!("git-remote-htree{suffix}")).exists())
        .collect();
    let path = std::env::join_paths(&paths).unwrap();
    let result = run(&haps, &path);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(fs::read_to_string(&marker).unwrap().contains("libexec"));
    let existing = tmp.path().join("existing");
    fs::create_dir(&existing).unwrap();
    fs::copy(&helper, existing.join(format!("git-remote-htree{suffix}"))).unwrap();
    let existing_path = std::env::join_paths(std::iter::once(existing).chain(paths)).unwrap();
    assert!(run(&haps, &existing_path).status.success());
    assert!(fs::read_to_string(&marker).unwrap().contains("existing"));
    let bare = tmp.path().join(format!("haps{suffix}"));
    fs::copy(&haps, &bare).unwrap();
    let missing = run(&bare, std::ffi::OsStr::new(""));
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("cargo install git-remote-htree"));
}

#[test]
fn htree_source_urls_accept_pinned_public_repos_only() {
    use haps::model::SourceInfo;
    let source = SourceInfo {
        git: "htree://npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/haps".into(),
        rev: "a".repeat(40),
    };
    source.validate().unwrap();
    for git in [
        "ext::sh -c something",
        "https://user:secret@example.org/repo",
        "htree://self/haps#k=secret",
    ] {
        assert!(
            SourceInfo {
                git: git.into(),
                ..source.clone()
            }
            .validate()
            .is_err()
        );
    }
}
