//! Orphan / held-resource detector — the "resource held open but idle"
//! drain class the wakes/CPU sampler misses entirely. A held audio sink,
//! an idle-inhibitor, wifi power_save left off, or a kernel wakelock each
//! cost hundreds of mW silently: CPU%, wakes/s and IO/s all read ~zero
//! while a hardware block is kept powered. The triggering incident was
//! `sd_dummy` (speech-dispatcher's dummy output) holding a PipeWire sink
//! open for 2d16h at near-zero CPU.
//!
//! Battery posture: scanned on a slow (30 s) cadence from the main tick,
//! skipped while paused. Every check is cheap — two short-lived
//! subprocesses (`pactl`, `systemd-inhibit`) and two stat+read of small
//! files. No background threads, no polling loop, no event plumbing.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OrphanClass {
    Audio,
    DbusInhibit,
    WifiPower,
    WakeLock,
}

impl OrphanClass {
    pub fn label(self) -> &'static str {
        match self {
            OrphanClass::Audio => "audio",
            OrphanClass::DbusInhibit => "inhibit",
            OrphanClass::WifiPower => "wifi-ps",
            OrphanClass::WakeLock => "wakelock",
        }
    }
    /// Rough midpoint of the per-class cost range, in mW. Deliberately
    /// approximate — it's a "how much is this worth chasing" hint, not a
    /// measurement (/proc/sysfs give no per-holder watt figure).
    fn base_mw(self) -> u32 {
        match self {
            OrphanClass::Audio => 225,       // 150–300
            OrphanClass::DbusInhibit => 350, // 200–500
            OrphanClass::WifiPower => 150,   // ~50–200 measured on this hw
            OrphanClass::WakeLock => 200,    // varies
        }
    }
}

pub struct Orphan {
    pub class: OrphanClass,
    pub pid: u32, // 0 when the class isn't pid-attributable (wifi, wakelock)
    pub cmd: String,
    pub detail: String,
    pub held_since: Instant,
    pub estimated_mw: u32,
    pub allowlisted: bool,
}

impl Orphan {
    fn new(class: OrphanClass, pid: u32, cmd: String, detail: String) -> Self {
        Orphan {
            class,
            pid,
            cmd,
            detail,
            held_since: Instant::now(),
            estimated_mw: class.base_mw(),
            allowlisted: false,
        }
    }
    fn new_nopid(class: OrphanClass, cmd: String, detail: String) -> Self {
        Self::new(class, 0, cmd, detail)
    }
    /// Stable allowlist / carry-forward key: class + cmd, no pid (so an
    /// allowlist entry survives the holder being restarted with a new pid).
    pub fn sig(&self) -> String {
        format!("{}:{}", self.class.label(), self.cmd)
    }
}

/// Run all detectors. `prev` is the previous scan's result, used to carry
/// each still-present orphan's `held_since` forward so the displayed
/// duration accumulates across the 30 s scans (a fresh detect always
/// stamps `now`). `allowlist` marks holders the user has chosen to ignore.
pub fn detect(prev: &[Orphan], allowlist: &HashMap<String, String>) -> Vec<Orphan> {
    let mut out = Vec::new();
    detect_audio(&mut out);
    detect_dbus_inhibit(&mut out);
    detect_wifi(&mut out);
    detect_wakelock(&mut out);
    for o in &mut out {
        if let Some(p) = prev.iter().find(|p| p.sig() == o.sig()) {
            o.held_since = p.held_since;
        }
        o.allowlisted = allowlist.contains_key(&o.sig());
    }
    out
}

/// SIGTERM (or SIGKILL when `hard`) the holder pid.
pub fn kill(pid: u32, hard: bool) -> std::io::Result<()> {
    if pid == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no pid to signal for this resource class",
        ));
    }
    let sig = if hard { libc::SIGKILL } else { libc::SIGTERM };
    let r = unsafe { libc::kill(pid as i32, sig) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn proc_comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{}/comm", pid))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("pid {}", pid))
}

/// Audio sink held active: a sink-input that is NOT corked is keeping the
/// sink (and the codec/DAC behind it) powered. `pactl list sink-inputs`
/// works through pipewire-pulse. The sd_dummy class lives here.
fn detect_audio(out: &mut Vec<Orphan>) {
    let output = match Command::new("pactl")
        .args(["list", "sink-inputs"])
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return,
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut corked = false;
    let mut have_corked = false;
    let mut pid = 0u32;
    let mut name = String::new();
    let mut in_block = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("Sink Input #") {
            if in_block {
                audio_push(corked, have_corked, pid, &name, out);
            }
            in_block = true;
            corked = false;
            have_corked = false;
            pid = 0;
            name = String::new();
        } else if let Some(v) = t.strip_prefix("Corked:") {
            corked = v.trim() == "yes";
            have_corked = true;
        } else if let Some(v) = t.strip_prefix("application.process.id = ") {
            pid = v.trim().trim_matches('"').parse().unwrap_or(0);
        } else if let Some(v) = t.strip_prefix("application.name = ") {
            name = v.trim().trim_matches('"').to_string();
        }
    }
    if in_block {
        audio_push(corked, have_corked, pid, &name, out);
    }
}

fn audio_push(corked: bool, have_corked: bool, pid: u32, name: &str, out: &mut Vec<Orphan>) {
    if have_corked && !corked {
        let cmd = if !name.is_empty() {
            name.to_string()
        } else if pid > 0 {
            proc_comm(pid)
        } else {
            "audio stream".to_string()
        };
        out.push(Orphan::new(
            OrphanClass::Audio,
            pid,
            cmd,
            "holding sink active (not corked)".to_string(),
        ));
    }
}

/// DBus idle-inhibitor: blocks the kernel's idle suspend. `systemd-inhibit
/// --list` covers the login1.Inhibit holders. We flag rows whose WHAT
/// includes idle/sleep. WHO can contain spaces, so we don't trust column
/// positions: scan for the idle/sleep token and take the 2nd numeric field
/// (UID, then PID) as the holder pid.
fn detect_dbus_inhibit(out: &mut Vec<Orphan>) {
    let output = match Command::new("systemd-inhibit")
        .args(["--list", "--no-pager"])
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return,
    };
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("WHO") || t.ends_with("listed.") {
            continue;
        }
        let fields: Vec<&str> = t.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        // MODE is the last field. Only block-mode idle inhibitors actually
        // hold the system out of idle suspend — delay-mode inhibitors
        // (NetworkManager, rtkit, UPower, browsers on the sleep/shutdown
        // signals) just postpone a transition briefly and are normal.
        if *fields.last().unwrap() != "block" {
            continue;
        }
        let what = match fields.iter().find(|f| f.split(':').any(|p| p == "idle")) {
            Some(w) => *w,
            None => continue,
        };
        let nums: Vec<u32> = fields.iter().filter_map(|f| f.parse::<u32>().ok()).collect();
        let pid = nums.get(1).copied().unwrap_or(0);
        if pid == 0 {
            continue;
        }
        out.push(Orphan::new(
            OrphanClass::DbusInhibit,
            pid,
            proc_comm(pid),
            format!("blocks idle ({})", what),
        ));
    }
}

/// WiFi power_save off keeps the radio out of its low-power doze. Read the
/// asmite state file (`/run/user/$UID/wifi-pwrsave`) — stat+read, no
/// process spawn. Absent file → nothing to report.
fn detect_wifi(out: &mut Vec<Orphan>) {
    let uid = unsafe { libc::getuid() };
    let path = format!("/run/user/{}/wifi-pwrsave", uid);
    if let Ok(s) = std::fs::read_to_string(&path) {
        // State byte written by ~/bin/wifi-pwrsave-state: '1' = power_save
        // ON (low-power), '0' = OFF (radio kept awake, ~50-200 mW).
        if s.trim() == "0" {
            out.push(Orphan::new_nopid(
                OrphanClass::WifiPower,
                "wifi power_save".to_string(),
                "radio kept awake (power_save off)".to_string(),
            ));
        }
    }
}

/// Kernel wakelock held: stat+read `/sys/power/wake_lock`; non-empty means
/// something is blocking autosleep. Absent on kernels without
/// CONFIG_PM_WAKELOCKS — then there's simply nothing to read.
fn detect_wakelock(out: &mut Vec<Orphan>) {
    if let Ok(s) = std::fs::read_to_string("/sys/power/wake_lock") {
        let locks = s.trim();
        if !locks.is_empty() {
            out.push(Orphan::new_nopid(
                OrphanClass::WakeLock,
                "kernel wakelock".to_string(),
                format!("held: {}", locks),
            ));
        }
    }
}
