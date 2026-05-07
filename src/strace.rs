//! `strace -c` wrapper: attach to a pid for N seconds and capture
//! the syscall histogram. Spawned in a thread so the TUI keeps
//! refreshing while strace runs.
//!
//! Format from strace (counts, time, errors per syscall) is parsed
//! into rows we can render in the analysis pane. strace -c needs
//! ptrace permission (kernel.yama.ptrace_scope) — if the attach
//! fails, the result includes the stderr message so the user knows.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct StraceResult {
    pub pid: u32,
    pub running: bool,
    pub error: Option<String>,
    pub rows: Vec<StraceRow>,
    pub total_calls: u64,
}

#[derive(Clone, Debug)]
pub struct StraceRow {
    pub pct_time: f64,
    pub seconds: f64,
    pub calls: u64,
    pub errors: u64,
    pub syscall: String,
}

/// Kicks off `strace -c -p PID` for ~3 seconds in a background
/// thread. The result Mutex is updated to `running=true` immediately
/// and overwritten with the parsed histogram when strace exits.
pub fn spawn_strace(slot: Arc<Mutex<StraceResult>>, pid: u32, secs: u64) {
    {
        let mut s = slot.lock().unwrap();
        *s = StraceResult {
            pid,
            running: true,
            error: None,
            rows: Vec::new(),
            total_calls: 0,
        };
    }
    std::thread::spawn(move || {
        // -c = histogram only (don't print every call). -p = attach.
        // strace writes the histogram to stderr on detach.
        let child = Command::new("strace")
            .arg("-c")
            .arg("-p")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let mut s = slot.lock().unwrap();
                *s = StraceResult {
                    pid,
                    running: false,
                    error: Some(format!("strace spawn failed: {}", e)),
                    rows: Vec::new(),
                    total_calls: 0,
                };
                return;
            }
        };
        std::thread::sleep(Duration::from_secs(secs));
        // SIGINT triggers strace's "print histogram and detach" path.
        unsafe {
            libc::kill(child.id() as i32, libc::SIGINT);
        }
        let mut stderr = String::new();
        if let Some(mut s) = child.stderr.take() {
            let _ = s.read_to_string(&mut stderr);
        }
        let _ = child.wait();
        let parsed = parse_histogram(&stderr);
        let mut s = slot.lock().unwrap();
        *s = StraceResult {
            pid,
            running: false,
            error: if parsed.0.is_empty() && stderr.contains("attach") {
                Some(stderr.lines().next().unwrap_or("attach failed").to_string())
            } else {
                None
            },
            total_calls: parsed.1,
            rows: parsed.0,
        };
    });
}

/// Parses strace -c stderr output of the form:
///     % time     seconds  usecs/call     calls    errors syscall
///     ------ ----------- ----------- --------- --------- ----------------
///      45.30    0.001234         123        10            poll
///       ...
///     ------ ----------- ----------- --------- --------- ----------------
///     100.00    0.002723                    25         0  total
fn parse_histogram(stderr: &str) -> (Vec<StraceRow>, u64) {
    let mut rows = Vec::new();
    let mut total_calls = 0u64;
    let mut in_table = false;
    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("% time") {
            in_table = true;
            continue;
        }
        if !in_table || trimmed.starts_with("------") {
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let cols: Vec<&str> = trimmed.split_whitespace().collect();
        if cols.is_empty() {
            continue;
        }
        if cols[cols.len() - 1] == "total" {
            // Total row: "% seconds usecs calls errors total"
            // We only want total_calls. cols[3] is calls.
            if let Some(c) = cols.get(3).and_then(|s| s.parse::<u64>().ok()) {
                total_calls = c;
            }
            continue;
        }
        // Need at least 5 cols: % seconds usecs calls syscall
        // (errors col may be absent if 0 — strace omits it sometimes).
        if cols.len() < 5 {
            continue;
        }
        let pct: f64 = cols[0].parse().unwrap_or(0.0);
        let secs: f64 = cols[1].parse().unwrap_or(0.0);
        // cols[2] is usecs/call (skip)
        let calls: u64 = cols[3].parse().unwrap_or(0);
        // Try parsing cols[4] as errors; if it fails (it's the syscall
        // name when errors are 0), then errors=0.
        let (errors, syscall_idx) = match cols[4].parse::<u64>() {
            Ok(e) => (e, 5),
            Err(_) => (0, 4),
        };
        if syscall_idx >= cols.len() {
            continue;
        }
        let syscall = cols[syscall_idx..].join(" ");
        rows.push(StraceRow {
            pct_time: pct,
            seconds: secs,
            calls,
            errors,
            syscall,
        });
    }
    rows.sort_by(|a, b| {
        b.calls
            .cmp(&a.calls)
            .then(b.pct_time.partial_cmp(&a.pct_time).unwrap_or(std::cmp::Ordering::Equal))
    });
    (rows, total_calls)
}
