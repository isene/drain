//! CHasm-suite summary: aggregate wakes/s and CPU% for the asm tools
//! (bare / glass / tile / strip / asmites). Pinned to the top of the
//! table as a quick "is the suite behaving?" glance — the original
//! use case drain was built for.

use crate::sample::Delta;

pub struct SuiteRow {
    pub name: &'static str,
    pub instances: u32,
    pub cpu_pct: f64,
    pub wakes_per_s: f64,
}

const TRACKED: &[&str] = &[
    "glass",
    "tile",
    "strip",
    "bare",
    "drain",
    // chasm-bits asmites
    "battery",
    "brightness",
    "clock",
    "cpu",
    "date",
    "disk",
    "ip",
    "mailbox",
    "mailbox-multi",
    "mailfetch",
    "mem",
    "moonphase",
    "ping",
    "pingok",
    "sep",
    "uptime",
    "wintitle",
];

pub fn summarize(deltas: &[Delta]) -> Vec<SuiteRow> {
    let mut rows: Vec<SuiteRow> = TRACKED
        .iter()
        .map(|n| SuiteRow {
            name: n,
            instances: 0,
            cpu_pct: 0.0,
            wakes_per_s: 0.0,
        })
        .collect();
    for d in deltas {
        if let Some(row) = rows.iter_mut().find(|r| r.name == d.comm) {
            row.instances += 1;
            row.cpu_pct += d.cpu_pct;
            row.wakes_per_s += d.wakes_per_s;
        }
    }
    rows.retain(|r| r.instances > 0);
    rows.sort_by(|a, b| {
        b.wakes_per_s
            .partial_cmp(&a.wakes_per_s)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

/// Render as a single line for the pinned summary row.
pub fn format_line(rows: &[SuiteRow]) -> String {
    if rows.is_empty() {
        return "  CHasm suite: (no tracked tools running)".to_string();
    }
    let mut parts: Vec<String> = rows
        .iter()
        .map(|r| {
            let inst = if r.instances > 1 {
                format!("×{}", r.instances)
            } else {
                String::new()
            };
            // Color by aggregate wakes/s. The asm suite design goal is
            // sub-10 wakes/s when idle — anything higher gets the
            // yellow/orange treatment.
            let col = if r.wakes_per_s >= 50.0 {
                196
            } else if r.wakes_per_s >= 20.0 {
                208
            } else if r.wakes_per_s >= 10.0 {
                226
            } else if r.wakes_per_s >= 1.0 {
                46
            } else {
                244
            };
            format!(
                "\x1b[38;5;{}m{}{} {:.1}w/s\x1b[0m",
                col, r.name, inst, r.wakes_per_s
            )
        })
        .collect();
    parts.insert(0, "\x1b[38;5;250msuite:\x1b[0m".to_string());
    format!("  {}", parts.join("  "))
}
