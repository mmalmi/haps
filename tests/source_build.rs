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
    let build = run(&["build", &url, "--rev", &rev, "--execute", "--install"]);
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
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
