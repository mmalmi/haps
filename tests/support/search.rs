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
const CLOSURE_BUDGET: Duration = Duration::from_secs(5);

// This narrow interface lets tests exercise OS errors and a stalled wait
// without leaving a real process alive or depending on scheduler races.
trait ClosingProcess {
    fn probe_reaped(&mut self) -> std::io::Result<bool>;
    fn send_kill(&mut self) -> std::io::Result<()>;
    async fn wait_reaped(&mut self) -> std::io::Result<()>;
}

impl ClosingProcess for tokio::process::Child {
    fn probe_reaped(&mut self) -> std::io::Result<bool> {
        self.try_wait().map(|status| status.is_some())
    }
    fn send_kill(&mut self) -> std::io::Result<()> {
        self.start_kill()
    }
    async fn wait_reaped(&mut self) -> std::io::Result<()> {
        self.wait().await.map(|_| ())
    }
}

async fn close(
    child: &mut impl ClosingProcess,
    pid: Option<u32>,
    budget: Duration,
    outcome: Result<()>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut errors = Vec::new();
    let mut reaped = match child.probe_reaped() {
        Ok(reaped) => reaped,
        Err(error) => {
            errors.push(format!("try_wait failed: {error:?}"));
            false
        }
    };
    if !reaped {
        if let Err(error) = child.send_kill() {
            errors.push(format!("start_kill failed: {error:?}"));
        }
        match tokio::time::timeout_at(deadline, child.wait_reaped()).await {
            Ok(Ok(())) => reaped = true,
            Ok(Err(error)) => errors.push(format!("wait failed: {error:?}")),
            Err(_) => errors.push(format!("closure deadline elapsed after {budget:?}")),
        }
    }
    if !reaped || !errors.is_empty() {
        let proof = if reaped {
            "reaped with cleanup errors"
        } else {
            "closure unproved"
        };
        let closure = format!("owned search PID {pid:?}: {proof}; {}", errors.join("; "));
        return match outcome {
            Ok(()) => Err(anyhow::anyhow!(closure)),
            Err(original) => Err(original.context(closure)),
        };
    }
    outcome
}

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
    let pid = child.id();
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
    let outcome = outcome
        .context("search exceeded its absolute deadline")
        .and_then(|result| result);
    close(&mut child, pid, CLOSURE_BUDGET, outcome)
        .await
        .with_context(|| {
            let last = serde_json::to_vec(&last)
                .unwrap_or_else(|error| format!("snapshot encoding failed: {error}").into_bytes());
            format!(
                "{app} cold search failed; final snapshot: {}; stderr: {}",
                preview(&last),
                preview(&captured)
            )
        })
}

#[cfg(test)]
mod closure_tests {
    use super::*;

    #[derive(Default)]
    struct Process {
        probe_error: bool,
        kill_error: bool,
        wait_error: bool,
        stalled_wait: bool,
        kills: usize,
        waits: usize,
    }

    impl ClosingProcess for Process {
        fn probe_reaped(&mut self) -> std::io::Result<bool> {
            if self.probe_error {
                Err(std::io::Error::other("probe fixture"))
            } else {
                Ok(false)
            }
        }
        fn send_kill(&mut self) -> std::io::Result<()> {
            self.kills += 1;
            if self.kill_error {
                Err(std::io::Error::other("kill fixture"))
            } else {
                Ok(())
            }
        }
        async fn wait_reaped(&mut self) -> std::io::Result<()> {
            self.waits += 1;
            if self.stalled_wait {
                std::future::pending::<()>().await;
            }
            if self.wait_error {
                Err(std::io::Error::other("wait fixture"))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn probe_error_still_kills_and_reaps_and_preserves_original_failure() {
        let mut process = Process {
            probe_error: true,
            ..Process::default()
        };
        let error = close(
            &mut process,
            Some(12345),
            Duration::from_secs(5),
            Err(anyhow::anyhow!("original search failure")),
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("probe fixture") && message.contains("original search failure"));
        assert!(message.contains("reaped with cleanup errors"));
        assert_eq!((process.kills, process.waits), (1, 1));
    }

    #[tokio::test]
    async fn kill_and_wait_errors_keep_owned_pid_and_prevent_acceptance() {
        let mut process = Process {
            kill_error: true,
            wait_error: true,
            ..Process::default()
        };
        let error = close(&mut process, Some(12345), Duration::from_secs(5), Ok(()))
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        for expected in ["12345", "closure unproved", "kill fixture", "wait fixture"] {
            assert!(message.contains(expected), "{message}");
        }
        assert_eq!((process.kills, process.waits), (1, 1));
    }

    #[tokio::test]
    async fn stalled_cleanup_has_its_own_deadline_and_retains_original_outcome() {
        let mut process = Process {
            stalled_wait: true,
            ..Process::default()
        };
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            close(
                &mut process,
                Some(12345),
                Duration::from_millis(10),
                Err(anyhow::anyhow!("original active deadline")),
            ),
        )
        .await
        .expect("cleanup exceeded the independent outer test bound")
        .unwrap_err();
        let message = format!("{error:#}");
        for expected in [
            "12345",
            "closure unproved",
            "closure deadline elapsed",
            "original active deadline",
        ] {
            assert!(message.contains(expected), "{message}");
        }
        assert_eq!((process.kills, process.waits), (1, 1));
    }
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
