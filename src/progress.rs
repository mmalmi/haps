//! Optional CLI progress. A separate reporter stays responsive during blocking I/O too.
use std::{
    io::{self, Write},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Default)]
pub struct Progress(Option<Arc<Shared>>);

pub struct Reporter(Option<(Arc<Shared>, JoinHandle<()>)>);

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

struct State {
    phase: Option<Phase>,
    last_output: Instant,
    stopped: bool,
}

struct Phase {
    label: String,
    started: Instant,
    transfer: Option<Transfer>,
}

struct Transfer {
    bytes: u64,
    total_bytes: u64,
    files: usize,
    total_files: usize,
}

impl Progress {
    pub fn stderr(enabled: bool) -> io::Result<(Self, Reporter)> {
        if !enabled {
            return Ok((Self::default(), Reporter(None)));
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                phase: None,
                last_output: Instant::now(),
                stopped: false,
            }),
            wake: Condvar::new(),
        });
        let worker_state = shared.clone();
        let worker = thread::Builder::new()
            .name("haps-progress".into())
            .spawn(move || {
                let mut state = worker_state.state.lock().unwrap();
                while !state.stopped {
                    let remaining = INTERVAL.saturating_sub(state.last_output.elapsed());
                    state = worker_state.wake.wait_timeout(state, remaining).unwrap().0;
                    if !state.stopped && state.last_output.elapsed() >= INTERVAL {
                        emit(&mut state);
                    }
                }
            })?;
        Ok((Self(Some(shared.clone())), Reporter(Some((shared, worker)))))
    }

    pub fn stage(&self, label: impl Into<String>) {
        self.set(Some(Phase {
            label: label.into(),
            started: Instant::now(),
            transfer: None,
        }));
    }

    pub fn clear(&self) {
        self.set(None);
    }

    fn set(&self, phase: Option<Phase>) {
        if let Some(shared) = &self.0 {
            let mut state = shared.state.lock().unwrap();
            state.phase = phase;
            emit(&mut state);
            shared.wake.notify_one();
        }
    }

    pub fn download(&self, total_bytes: u64, total_files: usize) {
        self.set(Some(Phase {
            label: "Downloading and verifying".into(),
            started: Instant::now(),
            transfer: Some(Transfer {
                bytes: 0,
                total_bytes,
                files: 0,
                total_files,
            }),
        }));
    }

    /// Count only bytes verified by the content store and written to staging.
    pub fn advance(&self, bytes: u64, files: usize) {
        if let Some(shared) = &self.0 {
            let mut state = shared.state.lock().unwrap();
            if let Some(transfer) = state.phase.as_mut().and_then(|p| p.transfer.as_mut()) {
                transfer.bytes += bytes;
                transfer.files += files;
            }
        }
    }

    pub fn finish_download(&self) {
        if let Some(shared) = &self.0 {
            emit(&mut shared.state.lock().unwrap());
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        if let Some((shared, worker)) = self.0.take() {
            shared.state.lock().unwrap().stopped = true;
            shared.wake.notify_one();
            let _ = worker.join();
        }
    }
}

fn emit(state: &mut State) {
    if let Some(phase) = &state.phase {
        let mut line = phase.label.clone();
        if let Some(t) = &phase.transfer {
            let percent = (t.bytes * 100)
                .checked_div(t.total_bytes)
                .or_else(|| (t.files as u64 * 100).checked_div(t.total_files as u64))
                .unwrap_or(100);
            line.push_str(&format!(
                ": {} / {} ({percent}%); {}/{} files",
                bytes(t.bytes),
                bytes(t.total_bytes),
                t.files,
                t.total_files
            ));
        } else {
            line.push_str("...");
        }
        let elapsed = phase.started.elapsed().as_secs();
        if elapsed > 0 {
            line.push_str(&format!(" ({elapsed}s elapsed)"));
        }
        let _ = writeln!(io::stderr().lock(), "  {line}");
    }
    state.last_output = Instant::now();
}

fn bytes(value: u64) -> String {
    for (size, unit) in [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")] {
        if value >= size {
            return format!("{:.1} {unit}", value as f64 / size as f64);
        }
    }
    format!("{value} B")
}
