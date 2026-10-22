use super::*;

pub(super) fn preflight() -> Result<()> {
    // Check namespaces before spending time fetching source or asking a reviewer.
    // This runs only the installed /bin/true, never project code, with a clean
    // environment and a read-only mount. The build gets the narrower mounts below.
    static RESULT: std::sync::OnceLock<std::result::Result<(), String>> =
        std::sync::OnceLock::new();
    let result = RESULT.get_or_init(|| {
        let output = Command::new("/usr/bin/bwrap")
            .args([
                "--unshare-all",
                "--die-with-parent",
                "--disable-userns",
                "--cap-drop",
                "ALL",
                "--ro-bind",
                "/",
                "/",
                "--",
                "/bin/true",
            ])
            .env_clear()
            .stdin(Stdio::null())
            .output()
            .map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(
                String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(4096)])
                    .into_owned(),
            )
        }
    });
    if let Err(error) = result {
        anyhow::bail!(
            "build isolation could not start: {}. Enable bubblewrap and unprivileged namespaces, or explicitly use --allow-host-build for a trusted build. No source was executed",
            error.escape_debug()
        );
    }
    Ok(())
}

pub(super) fn command(runner: &Runner, args: &[String]) -> Result<Command> {
    let mut command = Command::new("/usr/bin/bwrap");
    // Bubblewrap sets no_new_privs. All namespaces are private unless the user
    // explicitly grants host network access. Filesystem control sockets are not
    // mounted; --share-net also grants access to abstract Unix sockets.
    command.args([
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--disable-userns",
        "--cap-drop",
        "ALL",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--tmpfs",
        "/run",
    ]);
    if runner.execution.allow_network {
        command.arg("--share-net");
    }
    for path in &runner.readable {
        command.arg("--ro-bind").arg(path).arg(path);
    }
    command
        .arg("--bind")
        .arg(&runner.root)
        .arg(&runner.root)
        .arg("--bind")
        .arg(runner.scratch.path())
        .arg(runner.scratch.path())
        .arg("--chdir")
        .arg(&runner.root)
        .arg("--")
        .args(args);
    Ok(command)
}
