//! Best-effort copy-to-clipboard. Probes a list of platform-native
//! tools in order and pipes the text to the first one that's on
//! `$PATH`. We deliberately don't pull in a Rust clipboard crate
//! (most of them link X11 / Wayland system libs, which would break
//! the single-static-binary contract from CLAUDE.md / DESIGN).
//!
//! Tools tried, in priority order:
//!   1. `termux-clipboard-set`  (Termux + termux-api on Android)
//!   2. `wl-copy`               (Wayland desktops)
//!   3. `xclip -selection clipboard`  (X11)
//!   4. `xsel --clipboard --input`    (X11 alternative)
//!   5. `pbcopy`                (macOS)
//!   6. `clip.exe`              (WSL → Windows clipboard)
//!
//! On success returns the tool name so callers can show the user
//! what got used. On failure returns an error with a hint on what
//! to install for their platform.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};

/// Pipe `text` to the first available clipboard tool. Returns the
/// tool name (e.g. "wl-copy") so the caller can confirm to the user.
pub fn copy(text: &str) -> Result<&'static str> {
    for (bin, args) in CANDIDATES {
        if !on_path(bin) {
            continue;
        }
        let mut child = Command::new(bin)
            .args(args.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning {bin}"))?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(text.as_bytes())
                .with_context(|| format!("writing to {bin} stdin"))?;
        }
        let status = child.wait().with_context(|| format!("waiting on {bin}"))?;
        if status.success() {
            return Ok(*bin);
        }
        // If the tool exists but failed (e.g. termux-api not granted
        // permissions), keep trying others rather than giving up.
    }
    Err(anyhow!(
        "no clipboard tool on PATH. Install one for your platform:\n\
         - Termux: pkg install termux-api (and the Termux:API app)\n\
         - Wayland: install wl-clipboard\n\
         - X11: install xclip or xsel\n\
         - macOS: pbcopy is built-in (this case shouldn't happen)\n\
         - WSL: clip.exe is built-in (also shouldn't happen)"
    ))
}

/// `(binary_name, args_list)` table tried in order.
const CANDIDATES: &[(&str, &[&str])] = &[
    ("termux-clipboard-set", &[]),
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
    ("pbcopy", &[]),
    ("clip.exe", &[]),
];

fn on_path(bin: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return true;
        }
        #[cfg(windows)]
        for ext in ["exe", "cmd", "bat"] {
            if candidate.with_extension(ext).is_file() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_returns_error_when_no_tools_present() {
        // Wipe PATH for this thread's spawn — the candidate list
        // can't resolve so we expect an Err with the install hint.
        let saved = std::env::var_os("PATH");
        std::env::set_var("PATH", "/nonexistent-dir-for-pa-clipboard-test");
        let res = copy("hello");
        if let Some(p) = saved {
            std::env::set_var("PATH", p);
        }
        let err = res.unwrap_err().to_string();
        assert!(err.contains("no clipboard tool"), "got: {err}");
    }
}
