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
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
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
    paused: bool,
    deltas: Vec<Delta>,
    wsmap: HashMap<u32, u32>,
    winmap: Option<WinMap>,
    bat_avg: BatRing,
    show_help: bool,
    claude_text: Arc<Mutex<String>>,
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
            paused: false,
            deltas: Vec::new(),
            wsmap: HashMap::new(),
            winmap: WinMap::connect(),
            bat_avg: BatRing::new(),
            show_help: false,
            claude_text: Arc::new(Mutex::new(
                "(press c for analysis)".to_string(),
            )),
        }
    }

    fn tick(&mut self) {
        if self.paused {
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

    fn top_summary(&self, n: usize) -> String {
        let mut s = String::new();
        for d in self.deltas.iter().take(n) {
            let ws = match self.wsmap.get(&d.pid) {
                Some(&u32::MAX) => "?".to_string(),
                Some(n) => format!("{}", n + 1),
                None => "-".to_string(),
            };
            s.push_str(&format!(
                "{} (pid {}, ws {}): {:.1}% CPU, {:.1} wakes/s, {:.1} kB/s I/O\n",
                d.comm, d.pid, ws, d.cpu_pct, d.wakes_per_s, d.io_kbs
            ));
        }
        s
    }
}

fn dots(score: f64) -> String {
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
    let n = comm.chars().count();
    if n <= max {
        let mut s = comm.to_string();
        s.push_str(&" ".repeat(max - n));
        s
    } else {
        let mut s: String = comm.chars().take(max - 1).collect();
        s.push('…');
        s
    }
}

fn render_header(pane: &mut Pane, app: &App, cols: u16) {
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
    let paused = if app.paused { "  [PAUSED]" } else { "" };
    line.push_str(&format!(
        "    sort: {}{}",
        app.sort.label(),
        paused
    ));
    pad_to(&mut line, cols as usize);
    pane.set_text(&line);
    pane.refresh();
}

fn render_table(pane: &mut Pane, app: &App, rows: usize) {
    // Wider columns now that we have full screen width.
    //   PID(7) PROC(20) WS(3) CPU%(8) WAKE/s(10) NVOL/s(10) IO kB/s(10) DRAIN(7)
    let header = format!(
        " {:>6}  {:<20} {:>3}  \x1b[48;5;235m{:>7}\x1b[0m  {:>9}  {:>9}  {:>9}   DRAIN",
        "PID", "PROC", "WS", "CPU%", "WAKE/s", "NVOL/s", "IO kB/s"
    );
    let mut out = String::new();
    out.push_str(&format!("\x1b[1;38;5;250m{}\x1b[0m\n", header));
    let take = (rows.saturating_sub(2)).min(app.deltas.len());
    for d in app.deltas.iter().take(take) {
        let col = drain_color(d.drain);
        let ws = match app.wsmap.get(&d.pid) {
            Some(&u32::MAX) => "?".to_string(),
            Some(n) => format!("{}", n + 1),
            None => "·".to_string(),
        };
        let comm = comm_short(&d.comm, 20);
        let dot_str = dots(d.drain);
        // CPU% gets a dark-grey bg (235) so the column visually
        // anchors the eye when watching for spikes.
        let cpu_cell = format!("\x1b[48;5;235m {:>5.1}% \x1b[0m", d.cpu_pct);
        let line = format!(
            " {:>6}  {} {:>3}  {}  {:>9.1}  {:>9.1}  {:>9.1}   \x1b[38;5;{}m{}\x1b[0m",
            d.pid, comm, ws, cpu_cell, d.wakes_per_s, d.nvol_per_s, d.io_kbs, col, dot_str
        );
        out.push_str(&line);
        out.push('\n');
    }
    pane.set_text(out.trim_end_matches('\n'));
    pane.refresh();
}

fn render_claude(pane: &mut Pane, app: &App) {
    let txt = app.claude_text.lock().unwrap().clone();
    let header = "\x1b[1;38;5;250m─── analysis (c=refresh) ───\x1b[0m";
    pane.set_text(&format!("{}\n\n{}", header, txt));
    pane.refresh();
}

fn render_footer(pane: &mut Pane, app: &App, cols: u16) {
    let line = if app.show_help {
        format!(
            " q quit · s sort ({}) · p/f pause · c claude refresh · r reset baseline · h hide help",
            app.sort.label()
        )
    } else {
        format!(
            " q · s sort · p pause · c claude · r reset · h help    drain v{}",
            VERSION
        )
    };
    let mut padded = line;
    pad_to(&mut padded, cols as usize);
    pane.set_text(&padded);
    pane.refresh();
}

fn pad_to(s: &mut String, target: usize) {
    let visible = strip_ansi_for_width(s);
    let n = visible.chars().count();
    if n < target {
        s.push_str(&" ".repeat(target - n));
    }
}

fn strip_ansi_for_width(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
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

/// Spawn `claude -p` with a top-N drain summary as the prompt; write
/// the result into `claude_text` when the process returns. Runs on a
/// detached thread so the TUI keeps refreshing while claude thinks.
/// Slow (claude takes 5-30s); user-triggered, not on every refresh.
fn spawn_claude_query(claude_text: Arc<Mutex<String>>, summary: String) {
    {
        let mut t = claude_text.lock().unwrap();
        *t = "querying claude...".to_string();
    }
    std::thread::spawn(move || {
        let prompt = format!(
            "Brief analysis (3-5 sentences, no preamble) of these top \
             battery drainers from a Linux laptop. Note any obvious \
             polling-loop / hot-path candidates. The user runs a tiling \
             X11 session with custom asm tools (glass, tile, strip).\n\n{}",
            summary
        );
        let mut child = match Command::new("claude")
            .arg("-p")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                *claude_text.lock().unwrap() = format!("claude spawn failed: {}", e);
                return;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(prompt.as_bytes());
        }
        let output = match child.wait_with_output() {
            Ok(o) => o,
            Err(e) => {
                *claude_text.lock().unwrap() = format!("claude wait failed: {}", e);
                return;
            }
        };
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        *claude_text.lock().unwrap() = if text.is_empty() {
            "(claude returned empty)".to_string()
        } else {
            text
        };
    });
}

fn main() {
    Crust::init();
    // crossterm::terminal::size returns (cols, rows). crust passes
    // through unchanged. scribe destructures backwards but happens to
    // still work for its layout — we want it right.
    let (cols, rows) = Crust::terminal_size();
    let layout = compute_layout(cols, rows);
    let mut header = Pane::new(1, 1, cols, 1, 255, 236);
    let mut table = Pane::new(1, 2, layout.table_w, layout.body_h, 231, 0);
    let mut analysis = Pane::new(
        layout.table_w + 2,
        2,
        layout.analysis_w.saturating_sub(1),
        layout.body_h,
        231,
        0,
    );
    let mut footer = Pane::new(1, rows, cols, 1, 244, 236);
    analysis.wrap = true;
    let mut app = App::new();
    // Prime: take a second sample after a brief sleep so the first
    // visible frame has real deltas instead of "all 0.0".
    std::thread::sleep(std::time::Duration::from_millis(500));
    app.tick();
    // Initial claude assessment: a few sniffs of good data, then fire.
    std::thread::sleep(std::time::Duration::from_millis(800));
    app.tick();
    spawn_claude_query(Arc::clone(&app.claude_text), app.top_summary(8));

    loop {
        render_header(&mut header, &app, cols);
        render_table(&mut table, &app, layout.body_h as usize);
        render_claude(&mut analysis, &app);
        render_footer(&mut footer, &app, cols);
        let key = Input::getchr(Some(1));
        match key.as_deref() {
            Some("q") | Some("Q") => break,
            Some("s") | Some("S") => {
                app.sort = app.sort.next();
                app.sort_deltas();
            }
            Some("p") | Some("P") | Some("f") | Some("F") => app.paused = !app.paused,
            Some("c") | Some("C") => {
                spawn_claude_query(Arc::clone(&app.claude_text), app.top_summary(8));
            }
            Some("r") | Some("R") => {
                app.bat_avg = BatRing::new();
            }
            Some("h") | Some("H") | Some("?") => app.show_help = !app.show_help,
            Some("RESIZE") => {
                let (c2, r2) = Crust::terminal_size();
                let l2 = compute_layout(c2, r2);
                header = Pane::new(1, 1, c2, 1, 255, 236);
                table = Pane::new(1, 2, l2.table_w, l2.body_h, 231, 0);
                analysis = Pane::new(
                    l2.table_w + 2,
                    2,
                    l2.analysis_w.saturating_sub(1),
                    l2.body_h,
                    231,
                    0,
                );
                analysis.wrap = true;
                footer = Pane::new(1, r2, c2, 1, 244, 236);
            }
            _ => {}
        }
        app.tick();
    }
    Crust::cleanup();
}

struct Layout {
    table_w: u16,
    analysis_w: u16,
    body_h: u16,
}

fn compute_layout(cols: u16, rows: u16) -> Layout {
    // Give the table what it needs (~85 cols) and the analysis pane
    // gets the rest, with a 40-col minimum. On narrow terminals
    // (<120 cols) the analysis pane shrinks; on very narrow (<90)
    // it goes to 30 cols and the table truncates further.
    let table_w = if cols >= 120 {
        85
    } else if cols >= 90 {
        cols.saturating_sub(35)
    } else {
        cols.saturating_sub(30)
    };
    let analysis_w = cols.saturating_sub(table_w + 1);
    let body_h = rows.saturating_sub(2);
    Layout {
        table_w,
        analysis_w,
        body_h,
    }
}
