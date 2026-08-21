//! Battery tally — watt-hours accrued while nothing else is watching.
//!
//! The ledger in drain can only account for time drain is open. This
//! closes the gap without a timer: it blocks on the kernel's uevent
//! socket and wakes only when the power supply actually reports a
//! change, which is roughly once per percent plus each AC transition.
//! Idle cost is one sleeping process in recvmsg, no wakeups at all.
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
    let Some(fd) = uevent_socket() else {
        eprintln!("drain: cannot open the uevent socket");
        std::process::exit(1);
    };
    let mut buf = vec![0u8; 4096];
    let mut last = energy_wh(&dir);
    let mut date = today();
    let mut pending = 0.0f64;
    while wait_power_event(fd, &mut buf) {
        let now = energy_wh(&dir);
        let (Some(a), Some(b)) = (last, now) else { last = now; continue };
        // Only a fall while on battery is drain. A rise is charging, and
        // a fall while plugged in is the pack settling, not consumption.
        if discharging(&dir) && b < a {
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
