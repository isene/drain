//! /proc sampling: read every PID's CPU time, voluntary/involuntary
//! context switches, and I/O bytes. Two samples N seconds apart give
//! per-second rates. No `perf`, no root, no tracing — just /proc.

use std::collections::HashMap;
use std::fs;

#[derive(Clone, Debug, Default)]
pub struct Snap {
    pub utime: u64,        // /proc/<pid>/stat field 14 (clock ticks)
    pub stime: u64,        // /proc/<pid>/stat field 15 (clock ticks)
    pub vol_ctx: u64,      // voluntary_ctxt_switches
    pub nvol_ctx: u64,     // nonvoluntary_ctxt_switches
    pub read_bytes: u64,   // /proc/<pid>/io read_bytes
    pub write_bytes: u64,  // /proc/<pid>/io write_bytes
    pub comm: String,      // process name (15 chars max from comm)
    pub state: char,       // R/S/D/Z etc.
    pub ppid: u32,         // parent pid — used to resolve WS via ancestor walk
}

#[derive(Clone, Debug)]
pub struct Delta {
    pub pid: u32,
    pub comm: String,
    #[allow(dead_code)]
    pub state: char,
    pub ppid: u32,
    pub cpu_pct: f64,      // (utime + stime) per second / nproc / clock_tps * 100
    pub wakes_per_s: f64,  // voluntary_ctxt_switches/s — polling proxy
    pub nvol_per_s: f64,   // nonvoluntary_ctxt_switches/s
    pub io_kbs: f64,       // (read+write) bytes/s / 1024
    pub drain: f64,        // weighted score (higher = worse)
}

fn read_to_string_silent(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn parse_stat(content: &str) -> Option<(u64, u64, char, u32)> {
    // Format: pid (comm) state ppid ... utime stime ...
    // Find the closing paren, then split the rest.
    let close = content.rfind(')')?;
    let rest = &content[close + 2..]; // skip ") "
    let mut it = rest.split_ascii_whitespace();
    let state_s = it.next()?;
    let state = state_s.chars().next().unwrap_or('?');
    let ppid: u32 = it.next()?.parse().ok()?;
    // Skip pgrp, session, tty_nr, tpgid, flags, minflt, cminflt, majflt, cmajflt
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
            // "Name:\tfoo" or "voluntary_ctxt_switches:\t123"
            if let Some(num) = rest.trim_start_matches(':').trim().split_whitespace().next() {
                return num.parse().ok();
            }
        }
    }
    None
}

fn parse_io_field(content: &str, key: &str) -> Option<u64> {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            // "read_bytes: 12345"
            if let Some(num) = rest.trim_start_matches(':').trim().split_whitespace().next() {
                return num.parse().ok();
            }
        }
    }
    None
}

fn parse_comm(content: &str) -> String {
    // /proc/<pid>/stat: "1234 (proc name with spaces) S ..."
    if let Some(open) = content.find('(') {
        if let Some(close) = content.rfind(')') {
            if close > open {
                return content[open + 1..close].to_string();
            }
        }
    }
    String::new()
}

pub fn snapshot() -> HashMap<u32, Snap> {
    let mut out = HashMap::with_capacity(512);
    let dir = match fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return out,
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let pid: u32 = match name.to_string_lossy().parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let stat_path = format!("/proc/{}/stat", pid);
        let stat = match read_to_string_silent(&stat_path) {
            Some(s) => s,
            None => continue, // PID exited
        };
        let comm = parse_comm(&stat);
        let (utime, stime, state, ppid) = match parse_stat(&stat) {
            Some(t) => t,
            None => continue,
        };
        let status_path = format!("/proc/{}/status", pid);
        let (vol_ctx, nvol_ctx) = match read_to_string_silent(&status_path) {
            Some(s) => (
                parse_status_field(&s, "voluntary_ctxt_switches").unwrap_or(0),
                parse_status_field(&s, "nonvoluntary_ctxt_switches").unwrap_or(0),
            ),
            None => (0, 0),
        };
        let io_path = format!("/proc/{}/io", pid);
        let (read_bytes, write_bytes) = match read_to_string_silent(&io_path) {
            Some(s) => (
                parse_io_field(&s, "read_bytes").unwrap_or(0),
                parse_io_field(&s, "write_bytes").unwrap_or(0),
            ),
            // /proc/<pid>/io requires CAP_SYS_PTRACE for non-owned PIDs.
            // Drop quietly if denied.
            None => (0, 0),
        };
        out.insert(
            pid,
            Snap {
                utime,
                stime,
                vol_ctx,
                nvol_ctx,
                read_bytes,
                write_bytes,
                comm,
                state,
                ppid,
            },
        );
    }
    out
}

/// Compute per-process per-second deltas across two snapshots taken
/// `secs` seconds apart. Drops processes that exited between samples.
pub fn deltas(a: &HashMap<u32, Snap>, b: &HashMap<u32, Snap>, secs: f64, ncpus: f64) -> Vec<Delta> {
    let tps = clock_tps();
    let mut out = Vec::with_capacity(b.len());
    for (pid, s2) in b.iter() {
        let s1 = match a.get(pid) {
            Some(s) => s,
            // New process since last sample. Skip — no delta available
            // and a one-shot process wouldn't be a sustained drainer.
            None => continue,
        };
        let dt = s2
            .utime
            .saturating_sub(s1.utime)
            .saturating_add(s2.stime.saturating_sub(s1.stime)) as f64;
        let cpu_pct = (dt / tps) / secs * 100.0 / ncpus;
        let wakes = (s2.vol_ctx.saturating_sub(s1.vol_ctx)) as f64 / secs;
        let nvol = (s2.nvol_ctx.saturating_sub(s1.nvol_ctx)) as f64 / secs;
        let io_b = (s2.read_bytes.saturating_sub(s1.read_bytes)
            + s2.write_bytes.saturating_sub(s1.write_bytes)) as f64
            / secs;
        // Drain score: CPU% weighed heavier than wakes (a tight CPU
        // loop drains faster than 100 wakes/s of a sleeping process).
        // wakes/s scaled by 0.05 so 200 wakes/s ≈ 10 CPU% in the
        // ranking. I/O scaled to kB/s and weighed lightly.
        let drain = cpu_pct + wakes * 0.05 + (io_b / 1024.0) * 0.02;
        out.push(Delta {
            pid: *pid,
            comm: s2.comm.clone(),
            state: s2.state,
            ppid: s2.ppid,
            cpu_pct,
            wakes_per_s: wakes,
            nvol_per_s: nvol,
            io_kbs: io_b / 1024.0,
            drain,
        });
    }
    out.sort_by(|a, b| b.drain.partial_cmp(&a.drain).unwrap_or(std::cmp::Ordering::Equal));
    out
}

fn clock_tps() -> f64 {
    // SC_CLK_TCK is universally 100 on Linux but we ask anyway.
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) as f64 }
}

pub fn ncpus() -> f64 {
    unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) as f64 }
}

/// Read battery state from /sys/class/power_supply/BAT*/. Handles
/// both naming conventions:
///   energy domain  → power_now (µW), energy_now (µWh)
///   charge domain  → voltage_now (µV), current_now (µA), charge_now (µAh)
/// Returns (watts, hours_remaining, status_char). hours_remaining is
/// None when charging or when the data is incomplete.
pub fn battery() -> Option<(f64, Option<f64>, char)> {
    let dir = fs::read_dir("/sys/class/power_supply").ok()?;
    for entry in dir.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("BAT") {
            continue;
        }
        let base = entry.path();
        let read_u64 = |f: &str| -> Option<u64> {
            fs::read_to_string(base.join(f)).ok()?.trim().parse().ok()
        };
        let status = fs::read_to_string(base.join("status"))
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let st = status.chars().next().unwrap_or('?');

        // Energy-domain path (preferred when available — vendors that
        // expose it tend to be more accurate since they don't depend
        // on a separately-reported voltage).
        if let Some(power_uw) = read_u64("power_now") {
            let watts = power_uw as f64 / 1_000_000.0;
            let hours = if st == 'D' && watts > 0.0 {
                read_u64("energy_now").map(|e| (e as f64 / 1_000_000.0) / watts)
            } else {
                None
            };
            return Some((watts, hours, st));
        }

        // Charge-domain fallback. P = V × I; remaining = charge / current.
        let voltage = read_u64("voltage_now")?;
        let current = read_u64("current_now")?;
        // µV × µA = pW × 1e-12 = W. Compute as f64 to dodge u64 overflow.
        let watts = (voltage as f64 / 1_000_000.0) * (current as f64 / 1_000_000.0);
        let hours = if st == 'D' && current > 0 {
            read_u64("charge_now").map(|c| c as f64 / current as f64)
        } else {
            None
        };
        return Some((watts, hours, st));
    }
    None
}
