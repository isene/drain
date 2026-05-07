# drain — battery-drain triage TUI

![Version](https://img.shields.io/badge/version-0.1.0-blue) ![Rust](https://img.shields.io/badge/language-Rust-orange) ![License](https://img.shields.io/badge/license-Unlicense-green) ![Platform](https://img.shields.io/badge/platform-Linux-blue)

Small TUI that answers one question fast: **what is draining my
battery right now, and did I just make it worse?**

`top` and `htop` show CPU%. `powertop` is the kernel-level truth but
needs a tracing kernel, takes seconds to settle, and floods the
screen with what you didn't ask. `drain` reads `/proc` directly,
samples once per second, and ranks processes by a single drain score
combining CPU%, voluntary context switches/s (the polling proxy),
and I/O bytes/s.

Plus the killer feature when working on a tiling X11 session:
**WS column** — drain attributes each process to its workspace via
`_NET_WM_PID` + `_NET_WM_DESKTOP`, so you can tell *which* glass
terminal is the one running the rogue script.

## Why ctx-switches/s?

A polling loop wakes the CPU once per timer tick. Each wake counts
as one voluntary context switch in `/proc/<pid>/status`. So
`Δvoluntary_ctxt_switches / Δt` is the wakes-per-second rate — the
single best signal for "I introduced a poll where I should have used
an event wait".

A tight CPU loop also drains battery, so `cpu_pct` is included
(weighted higher than wakes — 1% CPU > 20 wakes/s in the score). I/O
gets a small weight too, since heavy I/O implies disk power draw.

## Usage

```
$ drain
```

```
 drain  Bat 6.42 W ↓  (Δ +1.30 W vs 30-sample avg)   4.8h left   sort: drain
   PID  PROC             WS   CPU%   WAKE/s   NVOL/s   IO kB/s  DRAIN
  1234  firefox           7   12.3    180.5      4.2       3.1  ●●●●●
  4567  slack             9    4.1     65.0      0.0       0.0  ●●●○○
  2233  glass             3    3.4     12.0      0.0       0.0  ●●○○○
  8901  tile              ·    0.8      2.0      0.0       0.0  ●○○○○
   ...
```

### Keys
| Key | Action                                                    |
|-----|-----------------------------------------------------------|
| `q` | quit                                                      |
| `s` | cycle sort (drain / cpu / wakes / io)                     |
| `f` | freeze (stops sampling so you can study the table)        |
| `r` | reset Δ-baseline (after a code change, restart the avg)   |
| `h` | toggle help line                                          |

### Workflow when battery W jumps from 4 → 7+

1. Open `drain`.
2. Press `s` to cycle to **wakes** sort. Top of the list is the
   suspect.
3. If it's one of your projects, press `f` to freeze, note the
   PID/comm, then dig into that source.
4. The WS column tells you which glass / firefox / slack instance
   it is when you have several.

## Build

```bash
cargo build --release
sudo install -m 0755 target/release/drain /usr/local/bin/drain
# or symlink:
ln -sf $(pwd)/target/release/drain ~/bin/drain
```

Static-linked dependencies: `crust` (TUI), `x11rb` (X11 wire
protocol for the WS column), `libc` (sysconf). No tracing kernel,
no root.

## Limitations

- `/proc/<pid>/io` is restricted on Linux — non-owned PIDs report 0
  for I/O without `CAP_SYS_PTRACE`. Fine for your own user-session
  drain hunting.
- `_NET_WM_PID` is set by most apps but not all (e.g. some Java
  Swing apps). Those show `·` in the WS column.
- Sub-second polling loops between samples are still visible (they
  show up as elevated wakes/s) but `drain` itself only refreshes
  once per second to keep its own footprint negligible.

## Roadmap

- v0.2: Enter on a row → expand threads (`/proc/<pid>/task/`).
- v0.2: `S` on a row → attach `strace -c` for 3s, show syscall
  histogram.
- v0.3: `d` diff mode → highlight processes whose wakes/s rate
  jumped vs. the rolling baseline.

Part of the [Fe2O3](https://github.com/isene/fe2o3) Rust terminal
suite.
