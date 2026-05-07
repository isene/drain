//! drain — battery-drain triage TUI.
//!
//! Sample /proc, compute per-second rates, render top drainers. The
//! key signal for "did I just introduce a poll loop?" is voluntary
//! context switches per second — a polling process wakes once per
//! tick. Combined with CPU% and battery W readout, you see what's
//! costing you watts without powertop's noise.

mod baseline;
mod clipboard;
mod sample;
mod strace;
mod suite;
mod threads;
mod winmap;

use baseline::BaselineSnapshot;
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

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Default top-N table.
    Table,
    /// Filter input is being typed; literal chars append to the
    /// filter string, Enter applies, Esc cancels.
    FilterEdit,
    /// Per-thread breakdown for one pid.
    Threads(u32),
    /// Strace histogram. The strace handle is in App.strace_slot.
    StraceView,
}

struct ThreadView {
    pid: u32,
    last_snap: HashMap<u32, Snap>,
    last_t: Instant,
    deltas: Vec<threads::ThreadDelta>,
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
    flash: Option<(String, Instant)>,
    selected: usize,
    filter: String,
    filter_input: String,
    interval_secs: f64,
    baseline: BaselineSnapshot,
    diff_mode: bool,
    mode: Mode,
    thread_view: Option<ThreadView>,
    strace_slot: Arc<Mutex<strace::StraceResult>>,
}

struct BatRing {
    samples: [f64; 30],
    n: usize,
    i: usize,
}

impl BatRing {
    fn new() -> Self {
        BatRing { samples: [0.0; 30], n: 0, i: 0 }
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
        let baseline = baseline::load();
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
            claude_text: Arc::new(Mutex::new("(press c for analysis)".to_string())),
            flash: None,
            selected: 0,
            filter: String::new(),
            filter_input: String::new(),
            interval_secs: 1.0,
            baseline,
            diff_mode: false,
            mode: Mode::Table,
            thread_view: None,
            strace_slot: Arc::new(Mutex::new(strace::StraceResult {
                pid: 0,
                running: false,
                error: None,
                rows: Vec::new(),
                total_calls: 0,
            })),
        }
    }

    fn tick(&mut self) {
        if self.paused {
            return;
        }
        let elapsed = self.last_t.elapsed().as_secs_f64();
        if elapsed < self.interval_secs * 0.9 {
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
        let bat = sample::battery();
        if let Some((w, _, _)) = bat {
            self.bat_avg.push(w);
        }
        // Update persistent baseline only on samples where the system
        // is genuinely "idle-ish" (no mass spike). EWMA inside the
        // baseline does the smoothing; we just feed every sample.
        self.baseline.update(&self.deltas, bat.map(|b| b.0));

        // If the user is in the threads view, also tick that.
        if let Some(tv) = &mut self.thread_view {
            let now_t = threads::snapshot_threads(tv.pid);
            tv.deltas = threads::deltas(&tv.last_snap, &now_t, elapsed, self.ncpus);
            tv.last_snap = now_t;
            tv.last_t = Instant::now();
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
            key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    fn filtered(&self) -> Vec<&Delta> {
        if self.filter.is_empty() {
            self.deltas.iter().collect()
        } else {
            self.deltas
                .iter()
                .filter(|d| d.comm.contains(&self.filter))
                .collect()
        }
    }

    fn top_summary(&self, n: usize) -> String {
        let mut s = String::new();
        for d in self.filtered().iter().take(n) {
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

    fn open_threads(&mut self, pid: u32) {
        let snap = threads::snapshot_threads(pid);
        self.thread_view = Some(ThreadView {
            pid,
            last_snap: snap,
            last_t: Instant::now(),
            deltas: Vec::new(),
        });
        self.mode = Mode::Threads(pid);
        self.selected = 0;
    }

    fn close_overlay(&mut self) {
        self.thread_view = None;
        self.mode = Mode::Table;
    }
}

fn dots(score: f64) -> String {
    let n = if score >= 50.0 { 5 } else if score >= 20.0 { 4 } else if score >= 8.0 { 3 } else if score >= 3.0 { 2 } else if score >= 1.0 { 1 } else { 0 };
    let mut s = String::with_capacity(5);
    for i in 0..5 {
        s.push(if i < n { '●' } else { '○' });
    }
    s
}

fn drain_color(score: f64) -> u16 {
    if score >= 20.0 { 196 } else if score >= 8.0 { 208 } else if score >= 3.0 { 226 } else if score >= 1.0 { 46 } else { 244 }
}

fn bat_color(watts: f64) -> u16 {
    if watts >= 10.0 { 196 } else if watts >= 7.0 { 208 } else if watts >= 5.0 { 226 } else { 46 }
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
        line.push_str(&format!("Bat \x1b[38;5;{}m{:.2} W\x1b[0m {}", col, w, arrow));
        if let Some(avg) = app.bat_avg.avg() {
            let delta = w - avg;
            let sign = if delta >= 0.0 { "+" } else { "" };
            line.push_str(&format!(
                "  (Δ {}{:.2} W vs {}-sample avg)",
                sign, delta, app.bat_avg.n
            ));
        }
        if app.baseline.samples_taken > 0 && app.baseline.bat_w_avg > 0.0 {
            let bd = w - app.baseline.bat_w_avg;
            let sign = if bd >= 0.0 { "+" } else { "" };
            line.push_str(&format!(
                "   |   baseline {}{:.2} W",
                sign, bd
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
    let mut tags = Vec::new();
    if app.paused {
        tags.push("PAUSED".to_string());
    }
    if app.diff_mode {
        tags.push("DIFF".to_string());
    }
    if !app.filter.is_empty() {
        tags.push(format!("filter:{}", app.filter));
    }
    if app.interval_secs != 1.0 {
        tags.push(format!("Δt:{:.1}s", app.interval_secs));
    }
    line.push_str(&format!(
        "    sort: {}",
        app.sort.label()
    ));
    if !tags.is_empty() {
        line.push_str(&format!(
            "    \x1b[1;38;5;226m[{}]\x1b[0m",
            tags.join(" ")
        ));
    }
    pad_to(&mut line, cols as usize);
    pane.set_text(&line);
    pane.refresh();
}

fn render_suite(pane: &mut Pane, app: &App, cols: u16) {
    let suite_rows = suite::summarize(&app.deltas);
    let line = suite::format_line(&suite_rows, cols as usize);
    pane.set_text(&line);
    pane.refresh();
}

fn render_table(pane: &mut Pane, app: &App, rows: usize) {
    let header = format!(
        " {:>8}  {:<20}  {:>2}  \x1b[48;5;235m {:>5} \x1b[0m  {:>8}  {:>8}  {:>8}  {:>5}",
        "PID", "PROC", "WS", "CPU%", "WAKE/s", "NVOL/s", "IO kB/s", "DRAIN"
    );
    let mut out = String::new();
    out.push_str(&format!("\x1b[1;38;5;250m{}\x1b[0m\n", header));
    let visible = app.filtered();
    let take = (rows.saturating_sub(2)).min(visible.len());
    for (i, d) in visible.iter().take(take).enumerate() {
        let col = drain_color(d.drain);
        let ws = match app.wsmap.get(&d.pid) {
            Some(&u32::MAX) => "?".to_string(),
            Some(n) => format!("{}", n + 1),
            None => "·".to_string(),
        };
        let comm = comm_short(&d.comm, 20);
        let dot_str = dots(d.drain);
        let cpu_cell = format!("\x1b[48;5;235m {:>5.1} \x1b[0m", d.cpu_pct);
        // Diff badge: when the per-comm wakes ratio is significantly
        // above baseline, prepend a colored marker. 1.5x = mild, 3x =
        // strong, 5x = "definitely look at this".
        let badge = if app.diff_mode {
            match app.baseline.wakes_anomaly(&d.comm, d.wakes_per_s) {
                Some(r) if r >= 5.0 => "\x1b[1;38;5;196m↑↑↑\x1b[0m".to_string(),
                Some(r) if r >= 3.0 => "\x1b[1;38;5;208m↑↑ \x1b[0m".to_string(),
                Some(r) if r >= 1.5 => "\x1b[1;38;5;226m↑  \x1b[0m".to_string(),
                _ => "   ".to_string(),
            }
        } else {
            "".to_string()
        };
        let line = format!(
            "{} {:>8}  {}  {:>2}  {}  {:>8.1}  {:>8.1}  {:>8.1}  \x1b[38;5;{}m{:>5}\x1b[0m",
            badge, d.pid, comm, ws, cpu_cell, d.wakes_per_s, d.nvol_per_s, d.io_kbs, col, dot_str
        );
        // Highlight selected row with a distinct background.
        let line = if i == app.selected {
            format!("\x1b[48;5;238m{}\x1b[0m", line)
        } else {
            line
        };
        out.push_str(&line);
        out.push('\n');
    }
    pane.set_text(out.trim_end_matches('\n'));
    pane.refresh();
}

fn render_threads(pane: &mut Pane, app: &App, _rows: usize) {
    let tv = match &app.thread_view {
        Some(t) => t,
        None => return,
    };
    let header_pid = format!(
        "\x1b[1;38;5;250m  Threads of pid {} ({})  —  Esc to go back\x1b[0m",
        tv.pid,
        app.deltas
            .iter()
            .find(|d| d.pid == tv.pid)
            .map(|d| d.comm.clone())
            .unwrap_or_default()
    );
    let table_header = format!(
        " {:>8}  {:<20}  {:>5}  {:>9}",
        "TID", "THREAD", "CPU%", "WAKE/s"
    );
    let mut out = String::new();
    out.push_str(&header_pid);
    out.push_str("\n\n");
    out.push_str(&format!("\x1b[1;38;5;250m{}\x1b[0m\n", table_header));
    for (i, t) in tv.deltas.iter().take(40).enumerate() {
        let line = format!(
            " {:>8}  {:<20}  {:>5.1}  {:>9.1}",
            t.tid,
            comm_short(&t.comm, 20),
            t.cpu_pct,
            t.wakes_per_s
        );
        let line = if i == app.selected {
            format!("\x1b[48;5;238m{}\x1b[0m", line)
        } else {
            line
        };
        out.push_str(&line);
        out.push('\n');
    }
    pane.set_text(out.trim_end_matches('\n'));
    pane.refresh();
}

fn render_analysis(pane: &mut Pane, app: &App) {
    let header = "\x1b[1;38;5;250m─── analysis ───\x1b[0m";
    let mut body = String::new();
    let strace_active = {
        let s = app.strace_slot.lock().unwrap();
        s.running || !s.rows.is_empty() || s.error.is_some()
    };
    if strace_active {
        let s = app.strace_slot.lock().unwrap();
        body.push_str(&format!(
            "strace -c -p {} ({})\n\n",
            s.pid,
            if s.running { "running…" } else { "done" }
        ));
        if let Some(e) = &s.error {
            body.push_str(&format!("\x1b[38;5;196merror:\x1b[0m {}\n\n", e));
        }
        if !s.rows.is_empty() {
            body.push_str(&format!(
                "  {:>5}  {:>9}  {:>5}  {}\n",
                "%time", "calls", "errs", "syscall"
            ));
            for r in s.rows.iter().take(15) {
                body.push_str(&format!(
                    "  {:>5.1}  {:>9}  {:>5}  {}\n",
                    r.pct_time, r.calls, r.errors, r.syscall
                ));
            }
            body.push_str(&format!("\n  total calls: {}\n", s.total_calls));
        }
    } else {
        body.push_str(&app.claude_text.lock().unwrap());
    }
    pane.set_text(&format!("{}\n\n{}", header, body));
    pane.refresh();
}

fn render_footer(pane: &mut Pane, app: &App, cols: u16) {
    let left = if app.mode == Mode::FilterEdit {
        format!(" /{}_  (Enter apply · Esc cancel)", app.filter_input)
    } else if let Some((msg, t)) = &app.flash {
        if t.elapsed().as_secs() < 3 {
            format!(" \x1b[38;5;46m{}\x1b[0m", msg)
        } else {
            keys_line(app)
        }
    } else {
        keys_line(app)
    };
    let right = format!("drain v{} ", VERSION);
    let lvis = strip_ansi_for_width(&left).chars().count();
    let rvis = strip_ansi_for_width(&right).chars().count();
    let total = cols as usize;
    let pad = total.saturating_sub(lvis + rvis);
    let line = format!("{}{}{}", left, " ".repeat(pad), right);
    pane.set_text(&line);
    pane.refresh();
}

fn keys_line(app: &App) -> String {
    if matches!(app.mode, Mode::Threads(_)) {
        return " ↑↓ select · Esc back".to_string();
    }
    if app.show_help {
        format!(
            " q quit · ↑↓ sel · Enter threads · S strace · s sort ({}) · d diff · / filter · +/- Δt · p pause · c claude · C-y copy · r reset · h hide help",
            app.sort.label()
        )
    } else {
        " q · ↑↓ · Enter · S · s · d · / · +/- · p · c · C-y · r · h help".to_string()
    }
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
    let (cols, rows) = Crust::terminal_size();
    let layout = compute_layout(cols, rows);
    let mut header = Pane::new(1, 1, cols, 1, 255, 236);
    let mut suite_pane = Pane::new(1, 2, cols, 1, 231, 234);
    let mut table = Pane::new(1, 3, layout.table_w, layout.body_h, 231, 0);
    let mut analysis = Pane::new(
        layout.table_w + 2,
        3,
        layout.analysis_w.saturating_sub(1),
        layout.body_h,
        231,
        0,
    );
    let mut footer = Pane::new(1, rows, cols, 1, 244, 236);
    analysis.wrap = true;
    let mut app = App::new();
    std::thread::sleep(std::time::Duration::from_millis(500));
    app.tick();
    std::thread::sleep(std::time::Duration::from_millis(800));
    app.tick();
    spawn_claude_query(Arc::clone(&app.claude_text), app.top_summary(8));

    loop {
        render_header(&mut header, &app, cols);
        render_suite(&mut suite_pane, &app, cols);
        match app.mode {
            Mode::Threads(_) => render_threads(&mut table, &app, layout.body_h as usize),
            _ => render_table(&mut table, &app, layout.body_h as usize),
        }
        render_analysis(&mut analysis, &app);
        render_footer(&mut footer, &app, cols);

        let key = Input::getchr(Some(1));
        let key_s = key.as_deref();

        // FilterEdit captures keystrokes for the filter string.
        if app.mode == Mode::FilterEdit {
            match key_s {
                Some("ENTER") => {
                    app.filter = app.filter_input.clone();
                    app.filter_input.clear();
                    app.mode = Mode::Table;
                    app.selected = 0;
                }
                Some("ESC") => {
                    app.filter_input.clear();
                    app.mode = Mode::Table;
                }
                Some("BACKSPACE") => {
                    app.filter_input.pop();
                }
                Some(s) if s.chars().count() == 1 => {
                    app.filter_input.push_str(s);
                }
                _ => {}
            }
            app.tick();
            continue;
        }

        match key_s {
            Some("q") | Some("Q") => break,
            Some("UP") => {
                app.selected = app.selected.saturating_sub(1);
            }
            Some("DOWN") => {
                let n_visible = match app.mode {
                    Mode::Threads(_) => app
                        .thread_view
                        .as_ref()
                        .map(|t| t.deltas.len())
                        .unwrap_or(0),
                    _ => app.filtered().len(),
                };
                if app.selected + 1 < n_visible {
                    app.selected += 1;
                }
            }
            Some("ENTER") => {
                if app.mode == Mode::Table {
                    if let Some(d) = app.filtered().get(app.selected).copied() {
                        let pid = d.pid;
                        app.open_threads(pid);
                    }
                }
            }
            Some("ESC") => {
                if matches!(app.mode, Mode::Threads(_) | Mode::StraceView) {
                    app.close_overlay();
                }
            }
            Some("S") => {
                if app.mode == Mode::Table {
                    if let Some(d) = app.filtered().get(app.selected).copied() {
                        strace::spawn_strace(Arc::clone(&app.strace_slot), d.pid, 3);
                        app.flash =
                            Some((format!("strace -c -p {} (3s)", d.pid), Instant::now()));
                    }
                }
            }
            Some("s") => {
                app.sort = app.sort.next();
                app.sort_deltas();
            }
            Some("d") | Some("D") => {
                app.diff_mode = !app.diff_mode;
                app.flash = Some((
                    format!(
                        "diff mode {} (baseline: {} samples)",
                        if app.diff_mode { "ON" } else { "off" },
                        app.baseline.samples_taken
                    ),
                    Instant::now(),
                ));
            }
            Some("/") => {
                app.mode = Mode::FilterEdit;
                app.filter_input = app.filter.clone();
            }
            Some("+") | Some("=") => {
                app.interval_secs = (app.interval_secs + 0.5).min(10.0);
                app.flash =
                    Some((format!("interval Δt = {:.1}s", app.interval_secs), Instant::now()));
            }
            Some("-") | Some("_") => {
                app.interval_secs = (app.interval_secs - 0.5).max(0.5);
                app.flash =
                    Some((format!("interval Δt = {:.1}s", app.interval_secs), Instant::now()));
            }
            Some("p") | Some("P") | Some("f") | Some("F") => app.paused = !app.paused,
            Some("c") | Some("C") => {
                spawn_claude_query(Arc::clone(&app.claude_text), app.top_summary(8));
            }
            Some("C-y") | Some("C-Y") => {
                let txt = app.claude_text.lock().unwrap().clone();
                clipboard::copy(&txt);
                app.flash = Some(("Copied analysis to clipboard.".to_string(), Instant::now()));
            }
            Some("r") | Some("R") => {
                app.bat_avg = BatRing::new();
                app.flash = Some(("Reset rolling average.".to_string(), Instant::now()));
            }
            Some("h") | Some("H") | Some("?") => app.show_help = !app.show_help,
            Some("RESIZE") => {
                let (c2, r2) = Crust::terminal_size();
                let l2 = compute_layout(c2, r2);
                header = Pane::new(1, 1, c2, 1, 255, 236);
                suite_pane = Pane::new(1, 2, c2, 1, 231, 234);
                table = Pane::new(1, 3, l2.table_w, l2.body_h, 231, 0);
                analysis = Pane::new(
                    l2.table_w + 2,
                    3,
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
    // Save baseline + bat ring on quit so the next session starts
    // with context. Best-effort — ignore failures.
    baseline::save(&app.baseline);
    Crust::cleanup();
}

struct Layout {
    table_w: u16,
    analysis_w: u16,
    body_h: u16,
}

fn compute_layout(cols: u16, rows: u16) -> Layout {
    let table_w = if cols >= 130 {
        90
    } else if cols >= 100 {
        cols.saturating_sub(40)
    } else {
        cols.saturating_sub(30)
    };
    let analysis_w = cols.saturating_sub(table_w + 1);
    // Rows used: header(1) + suite(1) + footer(1). Body rows for the
    // table + analysis split = rows - 3.
    let body_h = rows.saturating_sub(3);
    Layout { table_w, analysis_w, body_h }
}
