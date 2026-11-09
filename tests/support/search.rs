//! Bounded observation of one cold search process, with no retries or seeding.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    process::{Command, Stdio},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};

const SNAPSHOT_LIMIT: usize = 1024 * 1024;
const STDOUT_LIMIT: usize = 16 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;

async fn snapshots(reader: impl AsyncRead + Unpin, last: &mut Option<Vec<Value>>) -> Result<()> {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let mut total = 0;
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            if !line.is_empty() {
                *last = Some(serde_json::from_slice(&line).context("invalid search snapshot")?);
            }
            return Ok(());
        }
        let count = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        total += count;
        ensure!(total <= STDOUT_LIMIT, "search stdout exceeds 16 MiB");
        ensure!(
            line.len() + count <= SNAPSHOT_LIMIT,
            "search snapshot exceeds 1 MiB"
        );
        line.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if line.last() == Some(&b'\n') {
            *last = Some(serde_json::from_slice(&line).context("invalid search snapshot")?);
            line.clear();
        }
    }
}

async fn diagnostics(mut reader: impl AsyncRead + Unpin, captured: &mut Vec<u8>) -> Result<()> {
    let mut buffer = [0; 4096];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        ensure!(
            captured.len() + count <= STDERR_LIMIT,
            "search stderr exceeds 64 KiB"
        );
        captured.extend_from_slice(&buffer[..count]);
    }
}

fn preview(bytes: &[u8]) -> String {
    let shown = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    format!(
        "{shown}{}",
        if bytes.len() > 4096 {
            " [truncated]"
        } else {
            ""
        }
    )
}

pub async fn complete(
    command: Command,
    publisher: &str,
    app: &str,
    budget: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut child = tokio::process::Command::from(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().context("search stdout missing")?;
    let stderr = child.stderr.take().context("search stderr missing")?;
    let mut last = None;
    let mut captured = Vec::new();
    let outcome = tokio::time::timeout_at(deadline, async {
        let (_, _, status) = tokio::try_join!(
            snapshots(stdout, &mut last),
            diagnostics(stderr, &mut captured),
            async { Ok::<_, anyhow::Error>(child.wait().await?) },
        )?;
        ensure!(status.success(), "search exited with {status}");
        let rows = last.as_ref().context("search produced no snapshots")?;
        ensure!(
            rows.iter()
                .any(|item| item["publisher"] == publisher && item["package"]["name"] == app),
            "final search snapshot lacks expected publisher/package"
        );
        Ok(())
    })
    .await;
    // A parser error, output overflow or deadline must not leave a process or
    // pipe reader behind. kill() waits for the owned child to be reaped.
    if child.try_wait()?.is_none() {
        child
            .kill()
            .await
            .context("failed to kill and reap search")?;
    }
    let last = serde_json::to_vec(&last)?;
    outcome
        .context("search exceeded its absolute deadline")
        .and_then(|result| result)
        .with_context(|| {
            format!(
                "{app} cold search failed; final snapshot: {}; stderr: {}",
                preview(&last),
                preview(&captured)
            )
        })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const MATCH: &str = r#"[{"publisher":"fixture","package":{"name":"hello"}}]"#;

    fn fixture(script: &str, pid: &std::path::Path) -> Command {
        let mut command = Command::new("python3");
        command.args(["-u", "-c"])
            .arg(format!("import os, sys, time\nfrom pathlib import Path\nPath(sys.argv[1]).write_text(str(os.getpid()))\n{script}"))
            .arg(pid);
        command
    }

    fn reaped(pid: &std::path::Path) {
        let pid = std::fs::read_to_string(pid).unwrap();
        assert!(
            !Command::new("/bin/kill")
                .args(["-0", &pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success(),
            "owned search process remains alive"
        );
    }

    #[tokio::test]
    async fn final_match_after_snapshot_deadline_and_full_stderr_pipe() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let pid = temp.path().join("pid");
        let script = format!(
            "print('[]')\nsys.stderr.write('x' * 50000)\ntime.sleep(3.2)\nprint('{MATCH}')"
        );
        complete(
            fixture(&script, &pid),
            "fixture",
            "hello",
            Duration::from_secs(10),
        )
        .await?;
        reaped(&pid);
        Ok(())
    }

    #[tokio::test]
    async fn final_withdrawal_does_not_accept_an_earlier_match() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let pid = temp.path().join("pid");
        let error = complete(
            fixture(&format!("print('{MATCH}')\nprint('[]')"), &pid),
            "fixture",
            "hello",
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("final search snapshot lacks"));
        reaped(&pid);
        Ok(())
    }

    #[tokio::test]
    async fn failed_exit_invalid_json_and_overflow_fail_and_reap() -> Result<()> {
        for (script, expected) in [
            (
                format!("print('{MATCH}')\nsys.exit(7)"),
                "search exited with",
            ),
            (
                format!("print('{MATCH}')\nprint('invalid JSON')\ntime.sleep(60)"),
                "invalid search snapshot",
            ),
            (
                format!("print('{MATCH}')\nprint('{{}}')\ntime.sleep(60)"),
                "invalid search snapshot",
            ),
            (
                "print('x' * (1024 * 1024 + 1))\ntime.sleep(60)".into(),
                "snapshot exceeds",
            ),
            (
                "sys.stderr.write('x' * 70000)\ntime.sleep(60)".into(),
                "stderr exceeds",
            ),
            ("pass".into(), "search produced no snapshots"),
        ] {
            let temp = tempfile::tempdir()?;
            let pid = temp.path().join("pid");
            let error = complete(
                fixture(&script, &pid),
                "fixture",
                "hello",
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
            reaped(&pid);
        }
        Ok(())
    }

    #[tokio::test]
    async fn timeout_reaps_search_even_after_a_matching_snapshot() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let pid = temp.path().join("pid");
        let error = complete(
            fixture(
                &format!("print('{MATCH}')\nos.close(1)\nos.close(2)\ntime.sleep(60)"),
                &pid,
            ),
            "fixture",
            "hello",
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("absolute deadline"));
        reaped(&pid);
        Ok(())
    }
}
