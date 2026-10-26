#![cfg(target_os = "linux")]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn terminal_menu_can_run_a_nested_command_in_the_same_home() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let picker = bin.join("fzf");
    fs::write(
        &picker,
        "#!/bin/sh\ncat >/dev/null\nprintf 'list\\tInstalled packages\\n'\n",
    )
    .unwrap();
    fs::set_permissions(&picker, fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_haps"))
        .args(["--no-defaults", "--home"])
        .arg(root.path().join("data"))
        .args(["omarchy", "--menu"])
        .env("HOME", root.path())
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("HAPS_NO_DEFAULTS", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("terminal picker kept the Haps home locked");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("No installed packages."));
    assert!(!root.path().join(".config/omarchy").exists());
}
