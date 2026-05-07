//! Per-thread expansion: read /proc/<pid>/task/*/{stat,status} and
//! produce a Delta-like list at thread granularity. Used when the
//! user presses Enter on a row to dig into which thread of a multi-
//! threaded process is the actual culprit.

use crate::sample::Snap;
use std::collections::HashMap;
use std::fs;

#[derive(Clone, Debug)]
pub struct ThreadDelta {
    pub tid: u32,
    pub comm: String,
    pub cpu_pct: f64,
    pub wakes_per_s: f64,
}

pub fn snapshot_threads(pid: u32) -> HashMap<u32, Snap> {
    let mut out = HashMap::new();
    let task_dir = format!("/proc/{}/task", pid);
    let dir = match fs::read_dir(&task_dir) {
        Ok(d) => d,
        Err(_) => return out,
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let tid: u32 = match name.to_string_lossy().parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let stat_path = format!("{}/{}/stat", task_dir, tid);
        let stat = match fs::read_to_string(&stat_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let comm = parse_comm(&stat);
        let (utime, stime, state, ppid) = match parse_stat(&stat) {
            Some(t) => t,
            None => continue,
        };
        let status_path = format!("{}/{}/status", task_dir, tid);
        let (vol, nvol) = match fs::read_to_string(&status_path) {
            Ok(s) => (
                parse_status_field(&s, "voluntary_ctxt_switches").unwrap_or(0),
                parse_status_field(&s, "nonvoluntary_ctxt_switches").unwrap_or(0),
            ),
            Err(_) => (0, 0),
        };
        out.insert(
            tid,
            Snap {
                utime,
                stime,
                vol_ctx: vol,
                nvol_ctx: nvol,
                read_bytes: 0,
                write_bytes: 0,
                comm,
                state,
                ppid,
            },
        );
    }
    out
}

pub fn deltas(
    a: &HashMap<u32, Snap>,
    b: &HashMap<u32, Snap>,
    secs: f64,
    ncpus: f64,
) -> Vec<ThreadDelta> {
    let tps = unsafe { libc::sysconf(libc::_SC_CLK_TCK) as f64 };
    let mut out = Vec::with_capacity(b.len());
    for (tid, s2) in b.iter() {
        let s1 = match a.get(tid) {
            Some(s) => s,
            None => continue,
        };
        let dt = s2
            .utime
            .saturating_sub(s1.utime)
            .saturating_add(s2.stime.saturating_sub(s1.stime)) as f64;
        let cpu_pct = (dt / tps) / secs * 100.0 / ncpus;
        let wakes = (s2.vol_ctx.saturating_sub(s1.vol_ctx)) as f64 / secs;
        out.push(ThreadDelta {
            tid: *tid,
            comm: s2.comm.clone(),
            cpu_pct,
            wakes_per_s: wakes,
        });
    }
    out.sort_by(|a, b| {
        b.wakes_per_s
            .partial_cmp(&a.wakes_per_s)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn parse_stat(content: &str) -> Option<(u64, u64, char, u32)> {
    let close = content.rfind(')')?;
    let rest = &content[close + 2..];
    let mut it = rest.split_ascii_whitespace();
    let state_s = it.next()?;
    let state = state_s.chars().next().unwrap_or('?');
    let ppid: u32 = it.next()?.parse().ok()?;
    for _ in 0..9 {
        it.next()?;
    }
    let utime: u64 = it.next()?.parse().ok()?;
    let stime: u64 = it.next()?.parse().ok()?;
    Some((utime, stime, state, ppid))
}

fn parse_status_field(content: &str, key: &str) -> Option<u64> {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            if let Some(num) = rest.trim_start_matches(':').trim().split_whitespace().next() {
                return num.parse().ok();
            }
        }
    }
    None
}

fn parse_comm(content: &str) -> String {
    if let Some(open) = content.find('(') {
        if let Some(close) = content.rfind(')') {
            if close > open {
                return content[open + 1..close].to_string();
            }
        }
    }
    String::new()
}
