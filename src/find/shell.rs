//! Shared shell-out helpers for the find tiers (fd, zoxide,
//! locate). Two utilities:
//!
//! - `on_path`: probe whether a binary exists on `$PATH`. Used by
//!   each tier to skip cleanly if its tool isn't installed.
//! - `run_with_timeout`: spawn a `Command`, wait at most a fixed
//!   duration, and kill it if it overruns. Keeps a slow indexer or
//!   stuck `fd` from blocking the TUI's event loop.

use std::process::{Command, Output};
use std::time::Duration;

use crate::process::TimedOutput;

/// Is `bin` resolvable on `$PATH`? Mirrors the same check from
/// `crate::clipboard::on_path` — duplicated here rather than
/// re-exported because find/ should stand on its own.
pub fn on_path(bin: &str) -> bool {
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

/// Spawn `cmd`, polling for completion every 25 ms. If `timeout`
/// elapses we kill the child and return `None`. On normal completion
/// returns the captured `Output`.
///
/// We poll instead of using a spawn-and-wait helper crate because
/// the stdlib doesn't ship one and we don't want to take on `tokio`
/// or `wait-timeout` for this single use case (CLAUDE.md
/// single-static-binary preference).
pub fn run_with_timeout(cmd: Command, timeout: Duration) -> Option<Output> {
    match crate::process::run_with_timeout(cmd, timeout).ok()? {
        TimedOutput::Completed(output) => Some(output),
        TimedOutput::TimedOut => None,
    }
}
