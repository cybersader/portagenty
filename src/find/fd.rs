//! Tier 4: `fd`. Live directory walk that respects `.gitignore`
//! and is much faster than the stdlib fallback on large trees.
//!
//! Run as `fd --type d --hidden --exclude .git --exclude node_modules
//! --exclude target --max-depth N <query> <root>` per configured
//! root. Silent skip if `fd` isn't installed.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::find::shell;
use crate::find::FindOpts;

/// Run `fd` against each root with `query` as the pattern. Returns
/// matching directories.
pub fn collect(query: &str, opts: &FindOpts) -> Vec<PathBuf> {
    if query.is_empty() || !shell::on_path("fd") {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for root in &opts.roots {
        if !root.is_dir() {
            continue;
        }
        let mut cmd = Command::new("fd");
        cmd.arg("--type")
            .arg("d")
            .arg("--hidden")
            .arg("--exclude")
            .arg(".git")
            .arg("--exclude")
            .arg("node_modules")
            .arg("--exclude")
            .arg("target")
            .arg("--max-depth")
            .arg(opts.max_depth.to_string())
            .arg(query)
            .arg(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Some(output) = shell::run_with_timeout(cmd, Duration::from_secs(1)) else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let s = line.trim();
            if !s.is_empty() {
                out.push(PathBuf::from(s));
            }
        }
    }
    out
}
