//! Battery tally — watt-hours accrued while nothing else is watching.
//!
//! The ledger in drain can only account for time drain is open. This
//! closes the gap without a timer of its own.
//!
//! It listens to upower, which every desktop already runs and which
//! already samples the battery; its PropertiesChanged signal carries
//! Energy in watt-hours. Reading that feed costs one blocking read on a
//! pipe and adds no wakeups that were not happening anyway.
//!
//! Without upower it falls back to the kernel's uevent socket. That path
//! is honest but thin: this laptop's ACPI battery emits no uevent on a
//! capacity change (measured: none in seven minutes of discharge), so
//! the fallback catches AC transitions and little else.
//!
//! Energy comes from the battery's own counters, so a suspend is
//! measured too: the charge lost while asleep shows up in the first
//! event after resume. Rows land in the same ~/.drain/ledger.tsv the
//! TUI reads:
//!
//!     <date>\t__wh__\t<watt_hours>

use std::io::Write;
use std::path::{Path, PathBuf};

const NETLINK_KOBJECT_UEVENT: i32 = 15;
/// Flush once this much has accrued: a few appends an hour, and at most
/// this much is lost if the machine dies without a signal.
const FLUSH_WH: f64 = 0.25;
/// Above this draw the machine is doing something expensive, and it is
/// worth one process sample to find out what. Below it, nothing extra
/// happens at all. Override with DRAIN_PEAK_W.
const PEAK_W: f64 = 6.0;
/// Never sample more often than this, so a long expensive stretch costs
/// a handful of samples rather than one per signal.
const PEAK_GAP_S: u64 = 300;

fn bat_dir() -> Option<PathBuf> {
    let rd = std::fs::read_dir("/sys/class/power_supply").ok()?;
    for e in rd.flatten() {
        let p = e.path();
        if std::fs::read_to_string(p.join("type")).map(|t| t.trim() == "Battery")
            .unwrap_or(false)
        {
            return Some(p);
        }
    }
    None
}

fn read_num(dir: &Path, name: &str) -> Option<f64> {
    std::fs::read_to_string(dir.join(name)).ok()?.trim().parse::<f64>().ok()
}

/// Remaining energy in watt-hours. Batteries report either energy (µWh)
/// or charge (µAh) with a voltage; this laptop reports the latter.
fn energy_wh(dir: &Path) -> Option<f64> {
    if let Some(e) = read_num(dir, "energy_now") {
        return Some(e / 1e6);
    }
    let charge = read_num(dir, "charge_now")?;
    let volts = read_num(dir, "voltage_now")?;
    Some(charge * volts / 1e12)
}

fn discharging(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("status"))
        .map(|s| s.trim() == "Discharging")
        .unwrap_or(false)
}

fn today() -> String {
    let out = std::process::Command::new("date").arg("+%Y-%m-%d").output();
    out.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn append(date: &str, wh: f64) {
    if wh <= 0.0 {
        return;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let p = home.join(".drain").join("ledger.tsv");
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{}\t__wh__\t{:.3}", date, wh);
    }
}

/// Open the kernel uevent multicast socket. Nothing is sent on it until
/// a device changes, so the read below is a true sleep.
fn uevent_socket() -> Option<i32> {
    unsafe {
        let fd = libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                              NETLINK_KOBJECT_UEVENT);
        if fd < 0 {
            return None;
        }
        let mut addr: libc::sockaddr_nl = std::mem::zeroed();
        addr.nl_family = libc::AF_NETLINK as u16;
        addr.nl_groups = 1; // the kernel's own broadcast group
        let rc = libc::bind(fd, &addr as *const _ as *const libc::sockaddr,
                            std::mem::size_of::<libc::sockaddr_nl>() as u32);
        if rc < 0 {
            libc::close(fd);
            return None;
        }
        Some(fd)
    }
}

/// Block until the power supply reports something. False means the
/// socket died, which ends the run rather than spinning on a dead fd.
fn wait_power_event(fd: i32, buf: &mut [u8]) -> bool {
    loop {
        let n = unsafe {
            libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0)
        };
        if n <= 0 {
            return false;
        }
        let msg = String::from_utf8_lossy(&buf[..n as usize]);
        if msg.contains("power_supply") {
            return true;
        }
    }
}

/// One tally per machine, or the same watt-hours land in the ledger
/// twice. An abstract socket name holds the claim: it disappears with
/// the process, so there is no stale lock file to clean up.
fn claim_single_instance() -> bool {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return true; // cannot check: carry on rather than refuse to run
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as u16;
        let name = b"drain-tally";
        for (i, b) in name.iter().enumerate() {
            addr.sun_path[i + 1] = *b as libc::c_char; // [0] stays NUL: abstract
        }
        let len = std::mem::size_of::<libc::sa_family_t>() + 1 + name.len();
        libc::bind(fd, &addr as *const _ as *const libc::sockaddr, len as u32) == 0
    }
}

pub fn run() {
    if !claim_single_instance() {
        return; // another tally already has the battery
    }
    let Some(dir) = bat_dir() else {
        eprintln!("drain: no battery found");
        std::process::exit(1);
    };
    if run_upower(&dir) {
        return;
    }
    run_netlink(&dir)
}

/// Follow upower's battery signals. Each carries Energy in watt-hours,
/// so a fall while discharging is drain, measured rather than derived.
/// False means the feed never started, and the caller drops back to the
/// kernel socket.
fn run_upower(dir: &Path) -> bool {
    use std::io::BufRead;
    let child = std::process::Command::new("gdbus")
        .args(["monitor", "--system", "--dest", "org.freedesktop.UPower"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    let Some(out) = child.stdout.take() else { return false };
    let mut last: Option<f64> = None;
    let mut date = today();
    let mut pending = 0.0f64;
    let mut seen = false;
    let peak_w: f64 = std::env::var("DRAIN_PEAK_W").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(PEAK_W);
    let mut last_peak = 0u64;
    // PropertiesChanged carries only what changed, so a steady draw
    // stops being repeated. Remember the last rate we were told.
    let mut rate = 0.0f64;
    for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
        // The DisplayDevice repeats the same numbers; take the battery.
        if !line.contains("battery_") || !line.contains("'Energy':") {
            continue;
        }
        let Some(now) = field(&line, "'Energy': <") else { continue };
        seen = true;
        // Expensive right now: spend one sample on naming the cause.
        if let Some(w) = field(&line, "'EnergyRate': <") {
            rate = w;
        }
        let t = unix_now();
        if rate >= peak_w && discharging(dir)
            && t.saturating_sub(last_peak) >= PEAK_GAP_S
        {
            last_peak = t;
            record_peak(&date, rate);
        }
        if let Some(before) = last {
            if before > now && discharging(dir) {
                pending += before - now;
            }
        }
        last = Some(now);
        let d = today();
        if d != date {
            append(&date, pending);
            pending = 0.0;
            date = d;
        } else if pending >= FLUSH_WH {
            append(&date, pending);
            pending = 0.0;
        }
    }
    append(&date, pending);
    let _ = child.wait();
    seen
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One second of process accounting, written where the ledger and the
/// peaks file can use it. Runs only on an expensive wake, so the idle
/// path never pays for it.
fn record_peak(date: &str, watts: f64) {
    let a = crate::sample::snapshot();
    std::thread::sleep(std::time::Duration::from_secs(1));
    let b = crate::sample::snapshot();
    let mut d = crate::sample::deltas(&a, &b, 1.0, crate::sample::ncpus());
    d.sort_by(|x, y| y.cpu_pct.partial_cmp(&x.cpu_pct)
        .unwrap_or(std::cmp::Ordering::Equal));
    let top: Vec<&crate::sample::Delta> =
        d.iter().filter(|x| x.cpu_pct >= 0.1).take(5).collect();
    if top.is_empty() {
        return;
    }
    // CPU rows in the ledger's own format, so the per-app view has
    // something to apportion the day's watt-hours by.
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let led = home.join(".drain").join("ledger.tsv");
    if let Some(dir) = led.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&led) {
        for x in &top {
            let _ = writeln!(f, "{}\t{}\t{:.3}", date, x.comm, x.cpu_pct / 100.0);
        }
    }
    // And the stretch itself: when it was expensive, how expensive, who
    // was busy. This is the line that answers "what happened yesterday".
    let names: Vec<String> = top.iter()
        .map(|x| format!("{}:{:.0}%", x.comm, x.cpu_pct))
        .collect();
    let clock = std::process::Command::new("date").arg("+%Y-%m-%d %H:%M").output()
        .ok().and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string()).unwrap_or_default();
    let peaks = home.join(".drain").join("peaks.tsv");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&peaks) {
        let _ = writeln!(f, "{}\t{:.1}\t{}", clock, watts, names.join(" "));
    }
}

/// The number after `key` in a gdbus line: `'Energy': <15.97>`.
fn field(line: &str, key: &str) -> Option<f64> {
    let rest = line.split(key).nth(1)?;
    rest.split('>').next()?.trim().parse::<f64>().ok()
}

fn run_netlink(dir: &Path) {
    let Some(fd) = uevent_socket() else {
        eprintln!("drain: cannot open the uevent socket");
        std::process::exit(1);
    };
    let mut buf = vec![0u8; 4096];
    let mut last = energy_wh(dir);
    let mut date = today();
    let mut pending = 0.0f64;
    while wait_power_event(fd, &mut buf) {
        let now = energy_wh(dir);
        let (Some(a), Some(b)) = (last, now) else { last = now; continue };
        // Only a fall while on battery is drain. A rise is charging, and
        // a fall while plugged in is the pack settling, not consumption.
        if discharging(dir) && b < a {
            pending += a - b;
        }
        last = now;
        let d = today();
        if d != date {
            append(&date, pending);
            pending = 0.0;
            date = d;
        } else if pending >= FLUSH_WH {
            append(&date, pending);
            pending = 0.0;
        }
    }
    append(&date, pending);
}
