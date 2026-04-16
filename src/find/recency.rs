//! Tier 1: recency. Read recently-launched workspace files from
//! the state store and return their *parent directories* (since the
//! finder is looking for folders to scaffold *into*, and the stored
//! entry is the workspace TOML file path).
//!
//! Always-available, instant — no I/O beyond reading `state.toml`
//! once. Output is deduped on directory.

use std::collections::HashSet;
use std::path::PathBuf;

/// Up to N most-recent workspace dirs, in order.
pub fn collect() -> Vec<PathBuf> {
    let Ok(state) = crate::state::load() else {
        return Vec::new();
    };
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut out: Vec<PathBuf> = Vec::with_capacity(state.recent.len());
    for entry in &state.recent {
        let Some(dir) = entry.workspace_file.parent() else {
            continue;
        };
        let p = dir.to_path_buf();
        if seen.insert(p.clone()) {
            out.push(p);
        }
    }
    out
}
