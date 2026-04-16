//! Tier 5: stdlib walker. Always-available fallback. Recurses each
//! root up to `opts.max_depth` levels, skipping a small hardcoded
//! list of cache / build / VCS directories so we don't spend the
//! TUI's responsiveness budget crawling `node_modules`.
//!
//! No `.gitignore` parsing — that's `fd`'s job. We're the
//! everyone-has-this baseline. The hardcoded ignore list catches
//! the common offenders that aren't gitignored either (and would
//! still hurt walks even if they were).
//!
//! Output is *all* directories under the roots; the orchestrator's
//! ranker filters by query. Walking once and ranking is cheaper
//! than re-walking with a regex per keystroke.

use std::path::{Path, PathBuf};

use crate::find::FindOpts;

/// Directory names we skip unconditionally. Keep it short — every
/// addition makes the walker behavior slightly less predictable.
const IGNORE_NAMES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    ".cache",
    "venv",
    ".venv",
    "__pycache__",
    "dist",
    "build",
];

/// Walk each root up to `opts.max_depth` and return every visited
/// directory. The `_query` is currently unused at this layer (the
/// ranker filters by query), but keeping it in the signature lets a
/// future depth-aware optimization opt in (e.g. early-stop when no
/// children of a subtree could match a literal-prefix query).
pub fn collect(_query: &str, opts: &FindOpts) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for root in &opts.roots {
        if root.is_dir() {
            walk_into(root, 0, opts.max_depth, &mut out);
        }
    }
    out
}

fn walk_into(dir: &Path, depth: u16, max_depth: u16, out: &mut Vec<PathBuf>) {
    if depth > max_depth {
        return;
    }
    out.push(dir.to_path_buf());
    if depth == max_depth {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let name = e.file_name();
        let name_str = name.to_string_lossy();
        // Skip hidden dirs by default — most are dotfiles like
        // `.cache`, `.config`, etc. Users searching with leading `.`
        // intentionally still find them via the absolute-path mode
        // in the orchestrator.
        if name_str.starts_with('.') && name_str != "." {
            continue;
        }
        if IGNORE_NAMES.iter().any(|ign| *ign == name_str) {
            continue;
        }
        walk_into(&e.path(), depth + 1, max_depth, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::prelude::*;

    #[test]
    fn skips_node_modules_and_dot_git() {
        let tmp = assert_fs::TempDir::new().unwrap();
        tmp.child("project_a").create_dir_all().unwrap();
        tmp.child("project_a/node_modules").create_dir_all().unwrap();
        tmp.child("project_a/node_modules/lodash").create_dir_all().unwrap();
        tmp.child("project_a/.git").create_dir_all().unwrap();
        tmp.child("project_a/.git/objects").create_dir_all().unwrap();
        tmp.child("project_a/src").create_dir_all().unwrap();
        tmp.child("project_b").create_dir_all().unwrap();

        let opts = FindOpts {
            roots: vec![tmp.path().to_path_buf()],
            max_depth: 4,
            limit: 100,
        };
        let dirs = collect("", &opts);
        let names: Vec<String> = dirs
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        assert!(names.iter().any(|n| n == "project_a"));
        assert!(names.iter().any(|n| n == "project_b"));
        assert!(names.iter().any(|n| n == "src"));
        assert!(!names.iter().any(|n| n == "node_modules"), "got: {names:?}");
        assert!(!names.iter().any(|n| n == "lodash"), "got: {names:?}");
        assert!(!names.iter().any(|n| n == ".git"), "got: {names:?}");
        assert!(!names.iter().any(|n| n == "objects"), "got: {names:?}");
    }

    #[test]
    fn respects_max_depth() {
        let tmp = assert_fs::TempDir::new().unwrap();
        // 4 levels of nesting from tmp.
        tmp.child("a/b/c/d/e").create_dir_all().unwrap();

        // Depth 2 should reach `b` but not `d`.
        let opts = FindOpts {
            roots: vec![tmp.path().to_path_buf()],
            max_depth: 2,
            limit: 100,
        };
        let dirs = collect("", &opts);
        let names: Vec<String> = dirs
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        assert!(names.iter().any(|n| n == "a"), "got: {names:?}");
        assert!(names.iter().any(|n| n == "b"), "got: {names:?}");
        assert!(!names.iter().any(|n| n == "d"), "got: {names:?}");
    }
}
