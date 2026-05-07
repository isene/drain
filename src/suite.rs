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

// CHasm asm-suite processes. Excludes drain itself — drain polls /proc
// at 1Hz which is "the cost of monitoring", not a regression in the asm
// foundation. Showing drain here would always paint the suite line red
// for no actionable reason.
const TRACKED: &[&str] = &[
    "glass",
    "tile",
    "strip",
    "bare",
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

/// Render as a single line for the pinned summary row, truncated to
/// max_width visible chars and wrapped in a dark-grey bg so it stands
/// out from the column header below. The asm-suite design goal is
/// sub-10 wakes/s when idle, so the colour thresholds are calibrated
/// tighter than the main table:
///   ≥ 30 w/s   red    (something is running a tight loop)
///   ≥ 10 w/s   orange (active use OR mild regression)
///    ≥ 3 w/s   yellow (normal during typing / scrolling)
///    ≥ 1 w/s   green  (light activity)
///    < 1 w/s   dim    (idle — what we want)
pub fn format_line(rows: &[SuiteRow], max_width: usize) -> String {
    let bg = "\x1b[48;5;234m";
    let reset = "\x1b[0m";
    if rows.is_empty() {
        let body = "  suite: (no tracked asm tools running)";
        let pad_n = max_width.saturating_sub(body.chars().count());
        return format!("{}{}{}{}", bg, body, " ".repeat(pad_n), reset);
    }
    let mut parts: Vec<String> = rows
        .iter()
        .map(|r| {
            let inst = if r.instances > 1 {
                format!("×{}", r.instances)
            } else {
                String::new()
            };
            let col = if r.wakes_per_s >= 30.0 {
                196
            } else if r.wakes_per_s >= 10.0 {
                208
            } else if r.wakes_per_s >= 3.0 {
                226
            } else if r.wakes_per_s >= 1.0 {
                46
            } else {
                244
            };
            format!(
                "\x1b[38;5;{};48;5;234m{}{} {:.1}w/s\x1b[0m{}",
                col, r.name, inst, r.wakes_per_s, bg
            )
        })
        .collect();
    parts.insert(0, format!("\x1b[38;5;250;48;5;234msuite:\x1b[0m{}", bg));
    let body = format!("  {}", parts.join("  "));
    // Compute visible width so we can truncate-with-ellipsis when the
    // line exceeds the pane. Visible-width calc strips ANSI.
    let visible: String = strip_ansi(&body);
    let vis_n = visible.chars().count();
    let final_body = if vis_n <= max_width {
        let pad = max_width.saturating_sub(vis_n);
        format!("{}{}{}", body, " ".repeat(pad), reset)
    } else {
        // Walk visible chars up to max_width-1 and copy through ANSI
        // sequences as we go, then trail with "…" + bg-padded reset.
        let mut out = String::with_capacity(body.len());
        let mut count = 0;
        let mut in_esc = false;
        for c in body.chars() {
            if c == '\x1b' {
                in_esc = true;
                out.push(c);
                continue;
            }
            if in_esc {
                out.push(c);
                if c == 'm' {
                    in_esc = false;
                }
                continue;
            }
            if count >= max_width.saturating_sub(1) {
                break;
            }
            out.push(c);
            count += 1;
        }
        out.push('…');
        out.push_str(reset);
        out
    };
    final_body
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_esc = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_esc = true;
            continue;
        }
        if in_esc {
            if c == 'm' {
                in_esc = false;
            }
            continue;
        }
        out.push(c);
    }
    out
}
