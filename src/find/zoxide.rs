//! Tier 2: zoxide. If the user has `zoxide` on PATH (a common
//! shell `cd` alternative with frecency tracking), pull its sorted
//! list of dirs. Their frecency ranking is usually a much better
//! predictor of "what folder do I want to scaffold into" than a
//! cold filesystem walk.
//!
//! Silent skip when `zoxide` isn't installed — the orchestrator
//! treats an empty Vec as "this tier had nothing", not an error.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::find::shell;

/// Up to ~all entries from `zoxide query -l`, in zoxide's frecency
/// order (most-frecent first).
pub fn collect() -> Vec<PathBuf> {
    if !shell::on_path("zoxide") {
        return Vec::new();
    }
    let mut cmd = Command::new("zoxide");
    cmd.arg("query")
        .arg("-l")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Some(output) = shell::run_with_timeout(cmd, Duration::from_secs(1)) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}
