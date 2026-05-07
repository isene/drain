//! OSC 52 clipboard copy + base64 encoder. No xclip fork — just an
//! escape sequence the terminal interprets directly. Glass supports
//! it (vtp_osc52 path); same protocol as kitty / foot / alacritty /
//! xterm-with-allowWindowOps.

use std::io::Write;

const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn copy(text: &str) {
    let payload = b64_encode(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", payload);
    let _ = out.flush();
}

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(ABC[(b0 >> 2) as usize] as char);
        out.push(ABC[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ABC[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ABC[(b2 & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
