//! Finding config files on disk. See `DESIGN.md` §3.
//!
//! Three ways to find a workspace file:
//!
//! 1. **Walk-up from `$PWD`** — like `.git`. Starting from a dir, walk
//!    upward looking for any `*.portagenty.toml` with a non-empty prefix.
//!    The bare `portagenty.toml` name is always the per-project file and
//!    is explicitly excluded.
//! 2. **Global registry** — `GlobalFile::workspaces` lists known files,
//!    loaded via `config::files::load_toml`.
//! 3. **Explicit path** — the caller hands in `./foo.portagenty.toml`.
//!    No discovery; the path is used as-is. This is a trivial pass-through
//!    and lives at the call site.

use anyhow::{anyhow, Result};
use directories::ProjectDirs;
use std::path::{Path, PathBuf};

const WORKSPACE_SUFFIX: &str = ".portagenty.toml";
const PROJECT_FILENAME: &str = "portagenty.toml";

/// Predicate: does this filename look like a workspace file?
///
/// Requires a non-empty prefix before `.portagenty.toml`. Excludes the
/// bare per-project name `portagenty.toml` (which naturally fails the
/// suffix check because `portagenty.toml` does not end with
/// `.portagenty.toml`) and the dotfile-style `.portagenty.toml` (which
/// has exactly the suffix's length, failing the strict-greater check).
pub fn is_workspace_filename(name: &str) -> bool {
    name.len() > WORKSPACE_SUFFIX.len() && name.ends_with(WORKSPACE_SUFFIX)
}

/// First workspace file found in `dir`, sorted alphabetically for
/// determinism. Returns `None` if the dir can't be read or has no match.
pub fn workspace_in_dir(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut matches: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(is_workspace_filename)
                .unwrap_or(false)
        })
        .map(|e| e.path())
        .collect();
    matches.sort();
    matches.into_iter().next()
}

/// Walk up from `start`, returning the first workspace file found.
/// Returns `None` after reaching the filesystem root without a hit.
/// The returned path is the one found, not the directory containing it.
pub fn walk_up_from(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        if let Some(found) = workspace_in_dir(dir) {
            return Some(found);
        }
        cur = dir.parent();
    }
    None
}

/// Find a per-project `portagenty.toml` directly in `dir`.
pub fn project_file_in_dir(dir: &Path) -> Option<PathBuf> {
    let candidate = dir.join(PROJECT_FILENAME);
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// Resolved path to `$XDG_CONFIG_HOME/portagenty/config.toml` (or the
/// equivalent on macOS / Windows). Does not check existence — the caller
/// decides whether missing-global-config is an error or a silent default.
///
/// Checks `XDG_CONFIG_HOME` first on all platforms so that tests can
/// sandbox the config dir by setting the env var. The `directories`
/// crate ignores `XDG_CONFIG_HOME` on macOS (preferring
/// `~/Library/Application Support`), which breaks test isolation.
pub fn global_config_path() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("portagenty").join("config.toml"));
    }
    let dirs = ProjectDirs::from("", "", "portagenty")
        .ok_or_else(|| anyhow!("unable to resolve user config directory"))?;
    Ok(dirs.config_dir().join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::prelude::*;

    #[test]
    fn is_workspace_filename_accepts_prefixed_files() {
        assert!(is_workspace_filename("agentic.portagenty.toml"));
        assert!(is_workspace_filename("a.portagenty.toml"));
        assert!(is_workspace_filename("my-cool-workspace.portagenty.toml"));
    }

    #[test]
    fn is_workspace_filename_rejects_bare_project_file() {
        assert!(!is_workspace_filename("portagenty.toml"));
    }

    #[test]
    fn is_workspace_filename_rejects_dotfile_style() {
        assert!(!is_workspace_filename(".portagenty.toml"));
    }

    #[test]
    fn is_workspace_filename_rejects_non_matching() {
        assert!(!is_workspace_filename("random.toml"));
        assert!(!is_workspace_filename("portagenty"));
        assert!(!is_workspace_filename("portagenty.toml.bak"));
        assert!(!is_workspace_filename(""));
    }

    #[test]
    fn workspace_in_dir_finds_one() {
        let tmp = assert_fs::TempDir::new().unwrap();
        tmp.child("agentic.portagenty.toml").touch().unwrap();
        tmp.child("portagenty.toml").touch().unwrap(); // per-project; should be skipped
        tmp.child("noise.md").touch().unwrap();

        let found = workspace_in_dir(tmp.path()).unwrap();
        assert!(found.file_name().unwrap() == "agentic.portagenty.toml");
    }

    #[test]
    fn workspace_in_dir_picks_first_alphabetically_when_multiple() {
        let tmp = assert_fs::TempDir::new().unwrap();
        tmp.child("zzz.portagenty.toml").touch().unwrap();
        tmp.child("aaa.portagenty.toml").touch().unwrap();
        tmp.child("mmm.portagenty.toml").touch().unwrap();

        let found = workspace_in_dir(tmp.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "aaa.portagenty.toml");
    }

    #[test]
    fn workspace_in_dir_returns_none_when_empty() {
        let tmp = assert_fs::TempDir::new().unwrap();
        assert!(workspace_in_dir(tmp.path()).is_none());
    }

    #[test]
    fn walk_up_finds_workspace_in_current_dir() {
        let tmp = assert_fs::TempDir::new().unwrap();
        tmp.child("proj.portagenty.toml").touch().unwrap();
        let sub = tmp.child("sub");
        sub.create_dir_all().unwrap();

        let found = walk_up_from(sub.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "proj.portagenty.toml");
    }

    #[test]
    fn walk_up_walks_past_dirs_with_no_match() {
        let tmp = assert_fs::TempDir::new().unwrap();
        tmp.child("ws.portagenty.toml").touch().unwrap();
        let deep = tmp.child("a/b/c/d");
        deep.create_dir_all().unwrap();

        let found = walk_up_from(deep.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "ws.portagenty.toml");
    }

    #[test]
    fn walk_up_returns_none_when_nothing_up_the_tree() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let sub = tmp.child("sub");
        sub.create_dir_all().unwrap();
        // No *.portagenty.toml anywhere from `sub` up (the walk stops at
        // the filesystem root; no match is expected).
        assert!(walk_up_from(sub.path()).is_none());
    }

    #[test]
    fn walk_up_prefers_nearest_ancestor() {
        let tmp = assert_fs::TempDir::new().unwrap();
        tmp.child("outer.portagenty.toml").touch().unwrap();
        let inner = tmp.child("a/inner");
        inner.create_dir_all().unwrap();
        inner.child("inner.portagenty.toml").touch().unwrap();

        let found = walk_up_from(inner.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "inner.portagenty.toml");
    }

    #[test]
    fn project_file_in_dir_finds_bare_portagenty_toml() {
        let tmp = assert_fs::TempDir::new().unwrap();
        tmp.child("portagenty.toml").touch().unwrap();
        let found = project_file_in_dir(tmp.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "portagenty.toml");
    }

    #[test]
    fn project_file_in_dir_ignores_prefixed_files() {
        let tmp = assert_fs::TempDir::new().unwrap();
        tmp.child("stuff.portagenty.toml").touch().unwrap();
        assert!(project_file_in_dir(tmp.path()).is_none());
    }

    #[test]
    fn global_config_path_resolves() {
        let p = global_config_path().unwrap();
        // On every platform the resolved path should end with
        // "portagenty/config.toml" (or the platform's join equivalent).
        let s = p.to_string_lossy();
        assert!(
            s.contains("portagenty"),
            "path should contain 'portagenty': {s}"
        );
        assert!(
            s.ends_with("config.toml"),
            "path should end with config.toml: {s}"
        );
    }
}
