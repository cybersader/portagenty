//! Tier 3: pre-built filesystem index. Tries `plocate` (Linux,
//! modern), `locate` (Linux/BSD legacy), and `es.exe` (Everything
//! CLI on Windows) in order. First-on-PATH wins; the rest are
//! silently skipped.
//!
//! Returns *directories* only — locate-family tools index files
//! by default. We post-filter to keep entries whose existing
//! path-component points at a real directory.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::find::shell;

const CANDIDATES: &[(&str, &[&str])] = &[
    // plocate uses fnmatch-style globs by default; -i for
    // case-insensitive, --limit caps results.
    ("plocate", &["-i", "--limit", "200"]),
    ("locate", &["-i", "--limit", "200"]),
    // Everything CLI: -n caps results, -case-insensitive is default.
    ("es.exe", &["-n", "200"]),
];

/// Run the first available locate-style tool with `query` as its
/// pattern. Returns matching directories only.
pub fn collect(query: &str) -> Vec<PathBuf> {
    if query.is_empty() {
        return Vec::new();
    }
    for (bin, args) in CANDIDATES {
        if !shell::on_path(bin) {
            continue;
        }
        let mut cmd = Command::new(bin);
        cmd.args(args.iter().copied())
            .arg(query)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Some(output) = shell::run_with_timeout(cmd, Duration::from_secs(1)) else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        return stdout
            .lines()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .collect();
    }
    Vec::new()
}
