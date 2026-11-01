//! The power core: a warp core that pulses with the laptop's watts, and
//! a timeline of the day. The tally writes one line per upower reading
//! to ~/.drain/watts/YYYY-MM-DD.tsv; the `w` view reads it back.

use crust::style;
use std::io::Write;
use std::path::PathBuf;

/// One reading: seconds since local midnight, watts, on battery or not.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub secs: u32,
    pub watts: f32,
    pub dis: bool,
}

/// Days of watts files kept; older ones go at the tally's day change.
const KEEP_DAYS: i64 = 30;

/// Local date and seconds since midnight, without forking `date`.
pub fn local_now() -> (String, u32) {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    local_at(t)
}

fn local_at(t: i64) -> (String, u32) {
    // SAFETY: localtime_r only writes the tm we hand it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    unsafe { libc::localtime_r(&tt, &mut tm) };
    let date = format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday);
    (date, (tm.tm_hour * 3600 + tm.tm_min * 60 + tm.tm_sec) as u32)
}

fn dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".drain").join("watts")
}

/// Append one reading to the day's file. The folder is made only when
/// the first open fails, so a normal reading costs open, write, close.
pub fn log(date: &str, secs: u32, watts: f64, dis: bool) {
    let p = dir().join(format!("{date}.tsv"));
    let open = || std::fs::OpenOptions::new().create(true).append(true).open(&p);
    let f = open().or_else(|_| {
        std::fs::create_dir_all(dir())?;
        open()
    });
    if let Ok(mut f) = f {
        let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
        let st = if dis { 'D' } else { 'C' };
        let _ = writeln!(f, "{h:02}:{m:02}:{s:02}\t{watts:.2}\t{st}");
    }
}

/// Remove watts files older than KEEP_DAYS. Run once a day.
pub fn prune() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cutoff = local_at(now - KEEP_DAYS * 86400).0;
    let Ok(rd) = std::fs::read_dir(dir()) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.ends_with(".tsv") && name[..name.len() - 4] < *cutoff {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The battery's charge in percent.
pub fn capacity() -> Option<u8> {
    for e in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
        if e.file_name().to_string_lossy().starts_with("BAT") {
            return std::fs::read_to_string(e.path().join("capacity")).ok()?.trim().parse().ok();
        }
    }
    None
}

fn parse(line: &str) -> Option<Sample> {
    let mut f = line.split('\t');
    let mut hms = f.next()?.split(':').map(|x| x.parse::<u32>().ok());
    let secs = hms.next()?? * 3600 + hms.next()?? * 60 + hms.next()??;
    let watts = f.next()?.parse().ok()?;
    let dis = f.next()? == "D";
    Some(Sample { secs, watts, dis })
}

/// The day's readings in the order they were written.
pub fn read_day(date: &str) -> Vec<Sample> {
    let p = dir().join(format!("{date}.tsv"));
    std::fs::read_to_string(p)
        .map(|s| s.lines().filter_map(parse).collect())
        .unwrap_or_default()
}

/// Watt-hours on battery and the highest draw, from the readings.
/// A gap over ten minutes is a sleep and counts nothing.
pub fn day_sums(s: &[Sample]) -> (f64, Option<Sample>) {
    let mut wh = 0.0;
    for p in s.windows(2) {
        let dt = p[1].secs.saturating_sub(p[0].secs);
        if p[0].dis && dt <= 600 {
            wh += p[0].watts as f64 * dt as f64 / 3600.0;
        }
    }
    let peak = s.iter().filter(|x| x.dis).copied()
        .max_by(|a, b| a.watts.partial_cmp(&b.watts).unwrap_or(std::cmp::Ordering::Equal));
    (wh, peak)
}

/// Blue at low draw, amber by 8 W, red by 11 W: drain's own thresholds.
pub fn tint(w: f64) -> (f64, f64, f64) {
    const BLUE: (f64, f64, f64) = (60.0, 130.0, 255.0);
    const AMBER: (f64, f64, f64) = (255.0, 160.0, 30.0);
    const RED: (f64, f64, f64) = (255.0, 50.0, 40.0);
    let mix = |a: (f64, f64, f64), b: (f64, f64, f64), k: f64| {
        (a.0 + (b.0 - a.0) * k, a.1 + (b.1 - a.1) * k, a.2 + (b.2 - a.2) * k)
    };
    if w <= 5.0 {
        BLUE
    } else if w <= 8.0 {
        mix(BLUE, AMBER, (w - 5.0) / 3.0)
    } else if w <= 11.0 {
        mix(AMBER, RED, (w - 8.0) / 3.0)
    } else {
        RED
    }
}

const GREEN: (f64, f64, f64) = (70.0, 220.0, 140.0);

pub fn rgb8(c: (f64, f64, f64)) -> (u8, u8, u8) {
    (c.0.clamp(0.0, 255.0) as u8, c.1.clamp(0.0, 255.0) as u8, c.2.clamp(0.0, 255.0) as u8)
}

/// One pixel of the core at (x, y) in a w by h picture, or None for the
/// background. `phase` moves the bands, `t` flickers the chamber.
fn pixel(x: usize, y: usize, w: usize, h: usize, phase: f64, t: f64,
         c: (f64, f64, f64)) -> Option<(u8, u8, u8)> {
    let (wf, hf) = (w as f64, h as f64);
    let dx = (x as f64 - (wf - 1.0) / 2.0).abs();
    let cap = (h / 16).max(2);
    // LCARS caps above and below, a pill with its corners cut.
    if y < cap || y >= h - cap {
        let edge = y == 0 || y == h - 1;
        let half = wf * 0.45 - if edge { 1.5 } else { 0.0 };
        let colour = if y < cap { (255, 153, 0) } else { (204, 153, 204) };
        return (dx <= half).then_some(colour);
    }
    let r = (wf * 0.16).max(1.5);
    let mid = hf / 2.0;
    let d = (y as f64 - mid).abs();
    let ch = (hf * 0.09).max(3.0);
    let bulge = if d < ch { (1.0 - (d / ch).powi(2)).sqrt() } else { 0.0 };
    let re = r * (1.0 + 0.9 * bulge);
    let rail = r + 3.0;
    let shade = |b: f64| -> Option<(u8, u8, u8)> {
        let mut p = (c.0 * b, c.1 * b, c.2 * b);
        if b > 0.75 {
            let k = (b - 0.75) / 0.25 * 0.8;
            p = (p.0 + (255.0 - p.0) * k, p.1 + (255.0 - p.1) * k, p.2 + (255.0 - p.2) * k);
        }
        let q = rgb8(p);
        (q.0.max(q.1).max(q.2) >= 14).then_some(q)
    };
    if dx <= re + 0.5 {
        let prof = (1.0 - (dx / (re + 0.5)).powi(2)).max(0.0);
        if bulge > 0.0 {
            // The reaction chamber: white-hot in the middle, flickering.
            let flick = 0.5 + 0.5 * (t * 11.0 + (t * 3.7).sin() * 2.0).sin();
            return shade((0.55 + 0.45 * prof) * (0.8 + 0.2 * flick));
        }
        let wave = (0.5 + 0.5 * (std::f64::consts::TAU * (d / 9.0 + phase)).cos()).powi(2);
        let near = 0.7 + 0.3 * (1.0 - d / mid);
        let ring = if y % 5 == 0 { 0.55 } else { 1.0 };
        return shade(prof * (0.22 + 0.78 * wave) * near * ring);
    }
    if dx <= re + 1.5 {
        return shade(0.16);
    }
    if dx >= rail && dx < rail + 1.0 {
        return Some((96, 88, 124));
    }
    if dx < rail && y % 8 == 4 {
        return Some((58, 56, 76));
    }
    None
}

/// The core as `rows` lines of `cols` half-block cells. Each cell is two
/// pixels, the top as foreground of ▀ and the bottom as background; the
/// colour codes are written only where they change.
pub fn core(cols: usize, rows: usize, phase: f64, t: f64, state: char, watts: f64) -> Vec<String> {
    let c = match state {
        'C' => GREEN,
        'D' => tint(watts),
        _ => tint(0.0),
    };
    let h = rows * 2;
    let mut out = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut line = String::new();
        let (mut fg, mut bg): (Option<(u8, u8, u8)>, Option<(u8, u8, u8)>) = (None, None);
        for x in 0..cols {
            let a = pixel(x, row * 2, cols, h, phase, t, c);
            let b = pixel(x, row * 2 + 1, cols, h, phase, t, c);
            let (glyph, want_fg, want_bg) = match (a, b) {
                (None, None) => (' ', None, None),
                (Some(p), Some(q)) if p == q => (' ', None, Some(p)),
                (Some(p), None) => ('▀', Some(p), None),
                (None, Some(q)) => ('▄', Some(q), None),
                (Some(p), Some(q)) => ('▀', Some(p), Some(q)),
            };
            if want_bg.is_none() && bg.is_some() {
                line.push_str(style::RESET);
                fg = None;
                bg = None;
            }
            if let Some(q) = want_bg.filter(|q| bg != Some(*q)) {
                line.push_str(&style::set_bg_rgb(q.0, q.1, q.2));
                bg = Some(q);
            }
            if let Some(p) = want_fg.filter(|p| fg != Some(*p)) {
                line.push_str(&style::set_fg_rgb(p.0, p.1, p.2));
                fg = Some(p);
            }
            line.push(glyph);
        }
        if fg.is_some() || bg.is_some() {
            line.push_str(style::RESET);
        }
        out.push(line);
    }
    out
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Bin {
    Empty,
    Watts(f64),
    Charging,
}

/// Each slot of the day: the mean watts on battery, the charger, or
/// nothing (asleep, or still to come). A slot with no reading of its
/// own takes the last one, up to ten minutes back.
fn bins(s: &[Sample], n: usize, now: u32) -> Vec<Bin> {
    let span = 86400.0 / n as f64;
    let mut out = vec![Bin::Empty; n];
    let mut j = 0;
    let mut last: Option<Sample> = None;
    for (x, slot) in out.iter_mut().enumerate() {
        let start = (x as f64 * span) as u32;
        let end = ((x + 1) as f64 * span) as u32;
        let (mut sum, mut k, mut chg) = (0.0, 0, false);
        while j < s.len() && s[j].secs < end {
            if s[j].dis {
                sum += s[j].watts as f64;
                k += 1;
            } else {
                chg = true;
            }
            last = Some(s[j]);
            j += 1;
        }
        *slot = if k > 0 {
            Bin::Watts(sum / k as f64)
        } else if chg {
            Bin::Charging
        } else if start > now {
            Bin::Empty
        } else {
            match last {
                Some(l) if start.saturating_sub(l.secs) <= 600 => {
                    if l.dis { Bin::Watts(l.watts as f64) } else { Bin::Charging }
                }
                _ => Bin::Empty,
            }
        };
    }
    out
}

/// The day as a braille area chart: `rows` lines of plot, then the axis
/// and the hours. Charging shows as a green line along the bottom.
pub fn timeline(s: &[Sample], cols: usize, rows: usize, now: u32) -> Vec<String> {
    let plot = cols.saturating_sub(4).max(8);
    let b = bins(s, plot * 2, now);
    let top_w = b.iter().filter_map(|x| if let Bin::Watts(w) = x { Some(*w) } else { None })
        .fold(0.0, f64::max);
    let top = ((top_w / 4.0).ceil() * 4.0).max(8.0);
    let dots = rows * 4;
    let height = |x: &Bin| match x {
        Bin::Watts(w) => ((w / top * dots as f64).round() as usize).clamp(1, dots),
        Bin::Charging => 1,
        Bin::Empty => 0,
    };
    let mut out = Vec::with_capacity(rows + 2);
    for row in 0..rows {
        let label = if row == 0 {
            format!("{:>3}", top as u32)
        } else if row == rows / 2 {
            format!("{:>3}", (top / 2.0) as u32)
        } else {
            "   ".to_string()
        };
        let mut line = style::dim(&format!("{label}┤"));
        let mut cur: Option<(u8, u8, u8)> = None;
        // Dot rows counted from the bottom; this cell row covers four.
        let base = (rows - 1 - row) * 4;
        for cx in 0..plot {
            let mut bits = 0u32;
            let (mut wmax, mut chg) = (None::<f64>, false);
            for (side, bin) in [&b[cx * 2], &b[cx * 2 + 1]].into_iter().enumerate() {
                let hgt = height(bin);
                for k in 0..4 {
                    if base + k < hgt {
                        // Bit for dot row k from the bottom of the cell.
                        bits |= [[0x40, 0x04, 0x02, 0x01], [0x80, 0x20, 0x10, 0x08]][side][k];
                    }
                }
                match bin {
                    Bin::Watts(w) => wmax = Some(wmax.map_or(*w, |m| m.max(*w))),
                    Bin::Charging => chg = true,
                    Bin::Empty => {}
                }
            }
            if bits == 0 {
                line.push(' ');
                continue;
            }
            let colour = wmax.map(|w| rgb8(tint(w))).or(chg.then(|| rgb8(GREEN)));
            if let Some(c) = colour.filter(|c| cur != Some(*c)) {
                line.push_str(&style::set_fg_rgb(c.0, c.1, c.2));
                cur = Some(c);
            }
            line.push(char::from_u32(0x2800 + bits).unwrap_or(' '));
        }
        line.push_str(style::RESET);
        out.push(line);
    }
    // The axis, with the moment now marked, and the hours under it.
    let at = (now as usize * plot / 86400).min(plot - 1);
    let mut axis = style::dim(&format!("   └{}", "─".repeat(at)));
    axis.push_str(&style::rgb("▲", Some((255, 153, 0)), None, ""));
    axis.push_str(&style::dim(&"─".repeat(plot - 1 - at)));
    out.push(axis);
    let step = if plot >= 64 { 3 } else { 6 };
    let mut hours = vec![b' '; plot + 4];
    for hr in (0..=24).step_by(step) {
        let col = 4 + hr * (plot - 1) / 24;
        let txt = format!("{hr:02}");
        let col = col.min(plot + 2);
        hours[col..col + 2].copy_from_slice(txt.as_bytes());
    }
    out.push(style::dim(&String::from_utf8_lossy(&hours)));
    out
}

/// An LCARS heading: a rounded colour block with the name, then a bar.
pub fn pill(name: &str, c: (u8, u8, u8), width: usize) -> String {
    let label = format!(" {name} ");
    let bar = width.saturating_sub(label.chars().count() + 2);
    format!("{}{}{}",
        style::rgb("▐", Some(c), None, ""),
        style::rgb(&label, Some((0, 0, 0)), Some(c), "b"),
        style::rgb(&format!("▌{}", "━".repeat(bar)), Some(c), None, ""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible(s: &str) -> usize {
        crust::display_width(s)
    }

    #[test]
    fn lines_read_back() {
        assert_eq!(parse("10:42:07\t7.25\tD"),
                   Some(Sample { secs: 38527, watts: 7.25, dis: true }));
        assert_eq!(parse("00:00:01\t20.00\tC").map(|s| s.dis), Some(false));
        assert_eq!(parse("garbage"), None);
    }

    #[test]
    fn local_time_has_its_shape() {
        let (d, s) = local_now();
        assert_eq!(d.len(), 10);
        assert_eq!(&d[4..5], "-");
        assert!(s < 86400);
    }

    #[test]
    fn sums_skip_sleep_and_charging() {
        let s = [
            Sample { secs: 0, watts: 6.0, dis: true },
            Sample { secs: 600, watts: 6.0, dis: true },   // 6 W for 10 min = 1 Wh
            Sample { secs: 4200, watts: 9.0, dis: false }, // an hour asleep: nothing
            Sample { secs: 4800, watts: 9.0, dis: true },  // charging stretch: nothing
        ];
        let (wh, peak) = day_sums(&s);
        assert!((wh - 1.0).abs() < 1e-9);
        assert_eq!(peak.map(|p| p.watts), Some(9.0));
    }

    #[test]
    fn bins_fill_forward_but_not_into_the_future() {
        let s = [Sample { secs: 43200, watts: 8.0, dis: true }];
        let b = bins(&s, 24, 43200 + 1800);
        assert_eq!(b[11], Bin::Empty);
        assert_eq!(b[12], Bin::Watts(8.0));
        assert_eq!(b[13], Bin::Empty); // past ten minutes and past now
    }

    #[test]
    fn every_core_line_fills_its_width() {
        for state in ['D', 'C', 'F'] {
            let l = core(22, 30, 0.3, 1.0, state, 9.0);
            assert_eq!(l.len(), 30);
            assert!(l.iter().all(|x| visible(x) == 22));
        }
    }

    #[test]
    fn timeline_lines_fit() {
        let s = [Sample { secs: 30000, watts: 5.0, dis: true },
                 Sample { secs: 30060, watts: 12.0, dis: true }];
        let l = timeline(&s, 80, 8, 40000);
        assert_eq!(l.len(), 10);
        assert!(l.iter().all(|x| visible(x) <= 80));
        assert!(l[0].starts_with(&style::dim(" 12┤")));
    }
}
