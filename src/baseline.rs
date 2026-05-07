//! Persistent baseline + diff-mode anomaly detection.
//!
//! Saves the rolling battery-W average and a per-comm "what's normal"
//! snapshot to ~/.cache/drain/state.json on quit, reloads on start.
//! Diff mode then highlights processes whose wakes/s rate is well
//! above their own historical baseline — exactly the "did this code
//! change introduce polling?" check, without you needing to remember
//! the old number.

use crate::sample::Delta;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

#[derive(Clone, Debug, Default)]
pub struct BaselineSnapshot {
    pub bat_w_avg: f64,
    /// Median wakes/s per comm name. Comm is more stable than pid
    /// across restarts.
    pub wakes_median: HashMap<String, f64>,
    pub cpu_median: HashMap<String, f64>,
    pub samples_taken: u64,
}

impl BaselineSnapshot {
    /// Update the snapshot with a fresh window of deltas. Median is
    /// computed via a running EWMA approximation (alpha=0.1) — close
    /// enough for "what's typical" without keeping a history.
    pub fn update(&mut self, deltas: &[Delta], bat_w: Option<f64>) {
        let alpha = 0.1;
        if let Some(w) = bat_w {
            if self.samples_taken == 0 {
                self.bat_w_avg = w;
            } else {
                self.bat_w_avg = self.bat_w_avg * (1.0 - alpha) + w * alpha;
            }
        }
        for d in deltas {
            let key = d.comm.clone();
            let prev_w = self.wakes_median.get(&key).copied().unwrap_or(d.wakes_per_s);
            let prev_c = self.cpu_median.get(&key).copied().unwrap_or(d.cpu_pct);
            self.wakes_median
                .insert(key.clone(), prev_w * (1.0 - alpha) + d.wakes_per_s * alpha);
            self.cpu_median
                .insert(key, prev_c * (1.0 - alpha) + d.cpu_pct * alpha);
        }
        self.samples_taken = self.samples_taken.saturating_add(1);
    }

    /// Returns Some(severity) where 1.0 = matches baseline, 2.0 = 2×
    /// baseline, etc. None if no baseline yet for this comm.
    pub fn wakes_anomaly(&self, comm: &str, current: f64) -> Option<f64> {
        let base = self.wakes_median.get(comm).copied()?;
        if base < 1.0 {
            // Avoid division by ~zero. A process going from 0 → 50
            // wakes/s should still register as anomalous; treat
            // 0-baseline as 1 wake/s for the ratio.
            return Some(current / 1.0);
        }
        Some(current / base)
    }
}

fn cache_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cache/drain/state.json"))
}

/// Best-effort save. Failures are silently ignored — losing the
/// baseline isn't worse than a fresh install.
pub fn save(snap: &BaselineSnapshot) {
    let path = match cache_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let json = format_json(snap);
    if let Ok(mut f) = fs::File::create(&path) {
        let _ = f.write_all(json.as_bytes());
    }
}

pub fn load() -> BaselineSnapshot {
    let path = match cache_path() {
        Some(p) => p,
        None => return BaselineSnapshot::default(),
    };
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return BaselineSnapshot::default(),
    };
    parse_json(&content).unwrap_or_default()
}

/// Tiny hand-rolled JSON writer / reader. We only persist scalars and
/// flat string→f64 maps; pulling in serde for that is overkill given
/// the rest of drain has no serde dep.
fn format_json(s: &BaselineSnapshot) -> String {
    let mut out = String::from("{\n");
    out.push_str(&format!("  \"bat_w_avg\": {},\n", s.bat_w_avg));
    out.push_str(&format!("  \"samples_taken\": {},\n", s.samples_taken));
    out.push_str("  \"wakes_median\": {");
    let mut first = true;
    for (k, v) in s.wakes_median.iter() {
        if !first {
            out.push(',');
        }
        out.push_str(&format!("\n    {}: {}", quote(k), v));
        first = false;
    }
    out.push_str("\n  },\n");
    out.push_str("  \"cpu_median\": {");
    let mut first = true;
    for (k, v) in s.cpu_median.iter() {
        if !first {
            out.push(',');
        }
        out.push_str(&format!("\n    {}: {}", quote(k), v));
        first = false;
    }
    out.push_str("\n  }\n}\n");
    out
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn parse_json(s: &str) -> Option<BaselineSnapshot> {
    // Hand-rolled parser tuned for the shape format_json produces.
    // Robust enough for round-tripping our own files; not a general
    // JSON parser.
    let mut snap = BaselineSnapshot::default();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let key_end = bytes[i + 1..].iter().position(|&b| b == b'"')? + i + 1;
            let key = std::str::from_utf8(&bytes[i + 1..key_end]).ok()?;
            i = key_end + 1;
            // skip ":" + whitespace
            while i < bytes.len() && (bytes[i] == b':' || bytes[i].is_ascii_whitespace()) {
                i += 1;
            }
            match key {
                "bat_w_avg" => {
                    let (v, ni) = parse_number(bytes, i)?;
                    snap.bat_w_avg = v;
                    i = ni;
                }
                "samples_taken" => {
                    let (v, ni) = parse_number(bytes, i)?;
                    snap.samples_taken = v as u64;
                    i = ni;
                }
                "wakes_median" => {
                    let (m, ni) = parse_string_f64_map(bytes, i)?;
                    snap.wakes_median = m;
                    i = ni;
                }
                "cpu_median" => {
                    let (m, ni) = parse_string_f64_map(bytes, i)?;
                    snap.cpu_median = m;
                    i = ni;
                }
                _ => {
                    i += 1;
                }
            }
        } else {
            i += 1;
        }
    }
    Some(snap)
}

fn parse_number(bytes: &[u8], start: usize) -> Option<(f64, usize)> {
    let mut end = start;
    while end < bytes.len() && (bytes[end].is_ascii_digit() || matches!(bytes[end], b'.' | b'-' | b'e' | b'E' | b'+')) {
        end += 1;
    }
    let s = std::str::from_utf8(&bytes[start..end]).ok()?;
    let v = s.parse::<f64>().ok()?;
    Some((v, end))
}

fn parse_string_f64_map(bytes: &[u8], start: usize) -> Option<(HashMap<String, f64>, usize)> {
    let mut out = HashMap::new();
    let mut i = start;
    // Expect '{'
    while i < bytes.len() && bytes[i] != b'{' {
        i += 1;
    }
    i += 1;
    loop {
        // Skip whitespace and commas
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] == b'}' {
            return Some((out, i + 1));
        }
        if bytes[i] != b'"' {
            // Unexpected; bail.
            return Some((out, i));
        }
        let key_end = bytes[i + 1..].iter().position(|&b| b == b'"')? + i + 1;
        let key = std::str::from_utf8(&bytes[i + 1..key_end]).ok()?.to_string();
        i = key_end + 1;
        while i < bytes.len() && (bytes[i] == b':' || bytes[i].is_ascii_whitespace()) {
            i += 1;
        }
        let (v, ni) = parse_number(bytes, i)?;
        out.insert(key, v);
        i = ni;
    }
}
