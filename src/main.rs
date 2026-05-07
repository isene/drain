//! drain — battery-drain triage TUI.
//!
//! Sample /proc, compute per-second rates, render top drainers. The
//! key signal for "did I just introduce a poll loop?" is voluntary
//! context switches per second — a polling process wakes once per
//! tick. Combined with CPU% and battery W readout, you see what's
//! costing you watts without powertop's noise.

mod sample;
mod winmap;

use crust::{Crust, Input, Pane};
use sample::{Delta, Snap};
use std::collections::HashMap;
use std::time::Instant;
use winmap::WinMap;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Drain,
    Cpu,
    Wakes,
    Io,
}

impl SortBy {
    fn label(self) -> &'static str {
        match self {
            SortBy::Drain => "drain",
            SortBy::Cpu => "cpu",
            SortBy::Wakes => "wakes",
            SortBy::Io => "io",
        }
    }
    fn next(self) -> Self {
        match self {
            SortBy::Drain => SortBy::Cpu,
            SortBy::Cpu => SortBy::Wakes,
            SortBy::Wakes => SortBy::Io,
            SortBy::Io => SortBy::Drain,
        }
    }
}

struct App {
    last_snap: HashMap<u32, Snap>,
    last_t: Instant,
    ncpus: f64,
    sort: SortBy,
    frozen: bool,
    deltas: Vec<Delta>,
    wsmap: HashMap<u32, u32>,
    winmap: Option<WinMap>,
    bat_avg: BatRing,
    show_help: bool,
}

/// 30-sample ring buffer for "Δ vs avg" power readout. Catches a
/// step-change in idle power after a code change without requiring
/// the user to remember the old number.
struct BatRing {
    samples: [f64; 30],
    n: usize,
    i: usize,
}

impl BatRing {
    fn new() -> Self {
        BatRing {
            samples: [0.0; 30],
            n: 0,
            i: 0,
        }
    }
    fn push(&mut self, w: f64) {
        self.samples[self.i] = w;
        self.i = (self.i + 1) % self.samples.len();
        if self.n < self.samples.len() {
            self.n += 1;
        }
    }
    fn avg(&self) -> Option<f64> {
        if self.n == 0 {
            None
        } else {
            Some(self.samples[..self.n].iter().sum::<f64>() / self.n as f64)
        }
    }
}

impl App {
    fn new() -> Self {
        App {
            last_snap: sample::snapshot(),
            last_t: Instant::now(),
            ncpus: sample::ncpus(),
            sort: SortBy::Drain,
            frozen: false,
            deltas: Vec::new(),
            wsmap: HashMap::new(),
            winmap: WinMap::connect(),
            bat_avg: BatRing::new(),
            show_help: false,
        }
    }

    fn tick(&mut self) {
        if self.frozen {
            return;
        }
        let elapsed = self.last_t.elapsed().as_secs_f64();
        if elapsed < 0.9 {
            return;
        }
        let now = sample::snapshot();
        self.deltas = sample::deltas(&self.last_snap, &now, elapsed, self.ncpus);
        self.sort_deltas();
        self.last_snap = now;
        self.last_t = Instant::now();
        // Refresh WS map every tick; cost is ~10-30 X requests, all
        // cached in the X server. Worth it for live attribution.
        if let Some(wm) = &self.winmap {
            self.wsmap = wm.refresh();
        }
        if let Some((w, _, _)) = sample::battery() {
            self.bat_avg.push(w);
        }
    }

    fn sort_deltas(&mut self) {
        let key: fn(&Delta) -> f64 = match self.sort {
            SortBy::Drain => |d| d.drain,
            SortBy::Cpu => |d| d.cpu_pct,
            SortBy::Wakes => |d| d.wakes_per_s,
            SortBy::Io => |d| d.io_kbs,
        };
        self.deltas.sort_by(|a, b| {
            key(b)
                .partial_cmp(&key(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
}

fn dots(score: f64) -> String {
    // 5-dot drain visualisation. Thresholds tuned for "noticeable on
    // a 4W idle" — 1 dot at 1% or so, 5 dots well into a hot loop.
    let n = if score >= 50.0 {
        5
    } else if score >= 20.0 {
        4
    } else if score >= 8.0 {
        3
    } else if score >= 3.0 {
        2
    } else if score >= 1.0 {
        1
    } else {
        0
    };
    let mut s = String::with_capacity(5);
    for i in 0..5 {
        s.push(if i < n { '●' } else { '○' });
    }
    s
}

fn drain_color(score: f64) -> u16 {
    // Crust uses 256-color codes. Red 196, orange 208, yellow 226,
    // green 46, dim grey 244.
    if score >= 20.0 {
        196
    } else if score >= 8.0 {
        208
    } else if score >= 3.0 {
        226
    } else if score >= 1.0 {
        46
    } else {
        244
    }
}

fn bat_color(watts: f64) -> u16 {
    // 4W = baseline. 7W = suspicious. >10W = clearly bad.
    if watts >= 10.0 {
        196
    } else if watts >= 7.0 {
        208
    } else if watts >= 5.0 {
        226
    } else {
        46
    }
}

fn comm_short(comm: &str, max: usize) -> String {
    if comm.chars().count() <= max {
        format!("{:width$}", comm, width = max)
    } else {
        let mut s: String = comm.chars().take(max - 1).collect();
        s.push('…');
        s
    }
}

fn render_header(pane: &mut Pane, app: &App, cols: usize) {
    let bat = sample::battery();
    let mut line = String::new();
    line.push_str(" \x1b[1mdrain\x1b[0m  ");
    if let Some((w, hours, st)) = bat {
        let col = bat_color(w);
        let arrow = match st {
            'D' => "↓",
            'C' => "↑",
            'F' => "✓",
            _ => "·",
        };
        line.push_str(&format!(
            "Bat \x1b[38;5;{}m{:.2} W\x1b[0m {}",
            col, w, arrow
        ));
        if let Some(avg) = app.bat_avg.avg() {
            let delta = w - avg;
            let sign = if delta >= 0.0 { "+" } else { "" };
            line.push_str(&format!(
                "  (Δ {}{:.2} W vs {}-sample avg)",
                sign, delta, app.bat_avg.n
            ));
        }
        if let Some(h) = hours {
            line.push_str(&format!("   {:.1}h left", h));
        } else if st == 'C' {
            line.push_str("   charging");
        }
    } else {
        line.push_str("Bat n/a");
    }
    let frozen = if app.frozen { "  [FROZEN]" } else { "" };
    line.push_str(&format!(
        "    sort: {}{}",
        app.sort.label(),
        frozen
    ));
    let visible = strip_ansi_for_width(&line);
    let pad = cols.saturating_sub(visible.chars().count());
    line.push_str(&" ".repeat(pad));
    pane.set_text(&line);
    pane.refresh();
}

fn render_table(pane: &mut Pane, app: &App, rows: usize) {
    let header = format!(
        "{:>6}  {:<16} {:>2} {:>6}  {:>7}  {:>7}  {:>7}  DRAIN",
        "PID", "PROC", "WS", "CPU%", "WAKE/s", "NVOL/s", "IO kB/s"
    );
    let mut out = String::new();
    out.push_str(&format!("\x1b[1;38;5;250m{}\x1b[0m\n", header));
    let take = (rows.saturating_sub(2)).min(app.deltas.len());
    for d in app.deltas.iter().take(take) {
        let col = drain_color(d.drain);
        let ws = match app.wsmap.get(&d.pid) {
            Some(n) => format!("{}", n + 1),
            None => "·".to_string(),
        };
        let comm = comm_short(&d.comm, 16);
        let dot_str = dots(d.drain);
        let line = format!(
            "{:>6}  {} {:>2} {:>5.1}%  {:>7.1}  {:>7.1}  {:>7.1}  \x1b[38;5;{}m{}\x1b[0m",
            d.pid, comm, ws, d.cpu_pct, d.wakes_per_s, d.nvol_per_s, d.io_kbs, col, dot_str
        );
        out.push_str(&line);
        out.push('\n');
    }
    pane.set_text(out.trim_end_matches('\n'));
    pane.refresh();
}

fn render_footer(pane: &mut Pane, app: &App, cols: usize) {
    let line = if app.show_help {
        format!(
            " q quit · s sort ({}) · f freeze · r reset baseline · h hide help",
            app.sort.label()
        )
    } else {
        format!(
            " q · s sort · f freeze · r reset · h help    drain v{}",
            VERSION
        )
    };
    let visible_len = strip_ansi_for_width(&line).chars().count();
    let pad = cols.saturating_sub(visible_len);
    pane.set_text(&format!("{}{}", line, " ".repeat(pad)));
    pane.refresh();
}

/// Approximate visible-width count by stripping ANSI SGR. crust has
/// its own helper but it's not pub; this is good enough for padding.
fn strip_ansi_for_width(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // skip until 'm'
            i += 2;
            while i < bytes.len() && bytes[i] != b'm' {
                i += 1;
            }
            i += 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn main() {
    Crust::init();
    let (rows, cols) = Crust::terminal_size();
    let mut header = Pane::new(1, 1, cols, 1, 255, 236);
    let mut main_p = Pane::new(1, 2, cols, rows.saturating_sub(2), 231, 0);
    let mut footer = Pane::new(1, rows, cols, 1, 244, 236);
    let mut app = App::new();
    // Prime: take a second sample after a brief sleep so the first
    // visible frame has real deltas instead of "all 0.0".
    std::thread::sleep(std::time::Duration::from_millis(500));
    app.tick();

    loop {
        render_header(&mut header, &app, cols as usize);
        render_table(&mut main_p, &app, rows.saturating_sub(2) as usize);
        render_footer(&mut footer, &app, cols as usize);
        // 1s timeout: returns None when no input arrives, allowing
        // the next tick to pick up fresh deltas.
        let key = Input::getchr(Some(1));
        match key.as_deref() {
            Some("q") | Some("Q") => break,
            Some("s") | Some("S") => {
                app.sort = app.sort.next();
                app.sort_deltas();
            }
            Some("f") | Some("F") => app.frozen = !app.frozen,
            Some("r") | Some("R") => {
                // Reset baseline: discard the rolling average and
                // start fresh. Useful right after a code change so
                // "Δ vs avg" reflects the new state.
                app.bat_avg = BatRing::new();
            }
            Some("h") | Some("H") | Some("?") => app.show_help = !app.show_help,
            Some("RESIZE") => {
                let (r2, c2) = Crust::terminal_size();
                header = Pane::new(1, 1, c2, 1, 255, 236);
                main_p = Pane::new(1, 2, c2, r2.saturating_sub(2), 231, 0);
                footer = Pane::new(1, r2, c2, 1, 244, 236);
            }
            _ => {}
        }
        app.tick();
    }
    Crust::cleanup();
}
