//! Battery ledger — watt-hours and CPU-seconds per app per day.
//!
//! drain can only account for time it is actually open, and says so.
//! While ticking, each process is charged its CPU share of the interval;
//! the battery draw integrates to measured watt-hours. On quit the
//! session's totals append to ~/.drain/ledger.tsv:
//!
//!     <date>\t<comm>\t<cpu_seconds>
//!     <date>\t__wh__\t<watt_hours>
//!
//! The view aggregates the last seven days and apportions each day's
//! measured Wh by CPU share — an estimate, labelled as one.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

pub struct Ledger {
    /// cpu-seconds per comm, this session.
    pub cpu: HashMap<String, f64>,
    /// integrated watt-hours while discharging, this session.
    pub wh: f64,
    pub date: String,
}

fn path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".drain").join("ledger.tsv")
}

impl Ledger {
    pub fn new(date: String) -> Ledger {
        Ledger { cpu: HashMap::new(), wh: 0.0, date }
    }

    /// One tick's worth of accounting: dt seconds at `watts` draw,
    /// `procs` = (comm, cpu_pct) for the processes that did anything.
    pub fn tick(&mut self, dt: f64, watts: Option<f64>, procs: &[(String, f64)]) {
        if let Some(w) = watts {
            self.wh += w * dt / 3600.0;
        }
        for (comm, pct) in procs {
            if *pct > 0.1 {
                *self.cpu.entry(comm.clone()).or_insert(0.0) += pct / 100.0 * dt;
            }
        }
    }

    /// Append the session to the ledger file. Only comms with a second
    /// or more of CPU: the long tail is noise that bloats the file.
    pub fn save(&self) {
        if self.cpu.is_empty() && self.wh == 0.0 {
            return;
        }
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p)
        else { return };
        let mut rows: Vec<(&String, &f64)> = self.cpu.iter().collect();
        rows.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (comm, secs) in rows.into_iter().take(40) {
            if *secs >= 1.0 {
                let _ = writeln!(f, "{}\t{}\t{:.1}", self.date, comm, secs);
            }
        }
        if self.wh > 0.0 {
            let _ = writeln!(f, "{}\t__wh__\t{:.3}", self.date, self.wh);
        }
    }
}

pub struct Report {
    pub days: usize,
    pub total_wh: f64,
    /// (comm, cpu_seconds, estimated_wh) sorted by cpu.
    pub rows: Vec<(String, f64, f64)>,
}

/// Aggregate the ledger's last `days` distinct days.
/// The expensive stretches the tally recorded: when, how many watts,
/// and who was busy. Newest first, empty when the tally has not run.
pub fn peaks(n: usize) -> Vec<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let text = std::fs::read_to_string(home.join(".drain").join("peaks.tsv"))
        .unwrap_or_default();
    text.lines().rev().take(n).map(|l| l.replace('\t', "  ")).collect()
}

pub fn report(days: usize) -> Report {
    let text = std::fs::read_to_string(path()).unwrap_or_default();
    let mut dates: Vec<&str> = text
        .lines()
        .filter_map(|l| l.split('\t').next())
        .collect();
    dates.sort();
    dates.dedup();
    let keep: Vec<&str> = dates.into_iter().rev().take(days).collect();
    let mut cpu: HashMap<String, f64> = HashMap::new();
    let mut total_wh = 0.0;
    let mut total_cpu = 0.0;
    for l in text.lines() {
        let mut f = l.split('\t');
        let (Some(d), Some(comm), Some(v)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if !keep.contains(&d) {
            continue;
        }
        let v: f64 = v.parse().unwrap_or(0.0);
        if comm == "__wh__" {
            total_wh += v;
        } else {
            *cpu.entry(comm.to_string()).or_insert(0.0) += v;
            total_cpu += v;
        }
    }
    let mut rows: Vec<(String, f64, f64)> = cpu
        .into_iter()
        .map(|(c, s)| {
            let est = if total_cpu > 0.0 { total_wh * s / total_cpu } else { 0.0 };
            (c, s, est)
        })
        .collect();
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Report { days: keep.len(), total_wh, rows }
}
