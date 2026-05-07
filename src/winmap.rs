//! pid → workspace mapping via X11 wire protocol.
//!
//! Walks the root window's `_NET_CLIENT_LIST`, queries each window's
//! `_NET_WM_PID` and `_NET_WM_DESKTOP`, and builds a HashMap. Best-
//! effort: apps that don't set `_NET_WM_PID` (or set it to a wrong
//! pid) are simply absent from the map. This is fine for the use case
//! — we mainly want glass / firefox / slack attribution.

use std::collections::HashMap;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

pub struct WinMap {
    conn: RustConnection,
    root: u32,
    atom_client_list: Atom,
    atom_wm_pid: Atom,
    atom_wm_desktop: Atom,
}

impl WinMap {
    pub fn connect() -> Option<Self> {
        let (conn, screen_num) = RustConnection::connect(None).ok()?;
        let setup = conn.setup();
        let root = setup.roots[screen_num].root;
        let atom_client_list = intern(&conn, b"_NET_CLIENT_LIST")?;
        let atom_wm_pid = intern(&conn, b"_NET_WM_PID")?;
        let atom_wm_desktop = intern(&conn, b"_NET_WM_DESKTOP")?;
        Some(WinMap {
            conn,
            root,
            atom_client_list,
            atom_wm_pid,
            atom_wm_desktop,
        })
    }

    /// Build a {pid → workspace_index} map by walking _NET_CLIENT_LIST.
    /// Workspace numbering matches what tile / EWMH expose: 0-based.
    pub fn refresh(&self) -> HashMap<u32, u32> {
        let mut out = HashMap::new();
        let windows = match self.get_atom_array(self.root, self.atom_client_list, AtomEnum::WINDOW.into()) {
            Some(v) => v,
            None => return out,
        };
        for w in windows {
            let pid = self
                .get_atom_array(w, self.atom_wm_pid, AtomEnum::CARDINAL.into())
                .and_then(|v| v.first().copied());
            let desk = self
                .get_atom_array(w, self.atom_wm_desktop, AtomEnum::CARDINAL.into())
                .and_then(|v| v.first().copied());
            if let (Some(p), Some(d)) = (pid, desk) {
                // Keep the lowest workspace seen — if firefox spawns
                // sub-windows on different WSes, the first-found
                // wins. Good enough for the "which WS is burning" use
                // case where the user knows their layout.
                out.entry(p).or_insert(d);
            }
        }
        out
    }

    fn get_atom_array(&self, win: u32, prop: Atom, ty: Atom) -> Option<Vec<u32>> {
        let reply = self
            .conn
            .get_property(false, win, prop, ty, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        if reply.format != 32 {
            return None;
        }
        Some(
            reply
                .value
                .chunks_exact(4)
                .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        )
    }
}

fn intern(conn: &RustConnection, name: &[u8]) -> Option<Atom> {
    Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom)
}
