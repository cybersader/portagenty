//! Three-tier config loader: global + workspace + per-project. See
//! `DESIGN.md` §2.
//!
//! Public entry point is [`load`]; it wires discovery, file parsing,
//! and merge into a single resolved [`crate::domain::Workspace`].

pub mod discovery;
pub mod files;
pub mod merge;

pub use discovery::{
    global_config_path, is_workspace_filename, project_file_in_dir, walk_up_from, workspace_in_dir,
};
pub use files::{
    load_toml, GlobalFile, GlobalProjectEntry, GlobalWorkspaceEntry, ProjectFile, RawSession,
    WorkspaceFile,
};
pub use merge::{expand, resolve_path};

/// Read the current global default multiplexer, if any. Returns
/// `None` when the global config file doesn't exist yet OR when it
/// exists but doesn't pin a default.
pub fn current_default_multiplexer() -> Result<Option<crate::domain::Multiplexer>> {
    let path = match global_config_path() {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    if !path.is_file() {
        return Ok(None);
    }
    let global: GlobalFile = load_toml(&path)?;
    Ok(global.default_multiplexer)
}

/// Write (or update) the global default multiplexer in
/// `$XDG_CONFIG_HOME/portagenty/config.toml`. Uses toml_edit so any
/// other fields the user has set (project registrations, known
/// workspaces) are preserved verbatim. Creates the file + parent
/// dirs if they don't exist yet.
pub fn set_global_default_multiplexer(mpx: crate::domain::Multiplexer) -> Result<()> {
    let path = global_config_path()?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("parsing existing global config {}", path.display()))?;
    let wire = match mpx {
        crate::domain::Multiplexer::Tmux => "tmux",
        crate::domain::Multiplexer::Zellij => "zellij",
        crate::domain::Multiplexer::Wezterm => "wezterm",
    };
    doc["default-multiplexer"] = toml_edit::value(wire);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, doc.to_string())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Append a workspace file path to the global registry, idempotently.
/// Lets `pa` from any directory list known workspaces so users don't
/// have to walk into the project tree to see it. Preserves the rest
/// of the global config verbatim via toml_edit.
pub fn register_global_workspace(ws_path: &Path) -> Result<()> {
    let cfg_path = global_config_path()?;
    let existing = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("parsing existing global config {}", cfg_path.display()))?;

    let canonical = ws_path
        .canonicalize()
        .unwrap_or_else(|_| ws_path.to_path_buf());
    let wanted = canonical.display().to_string();

    // Walk existing [[workspace]] entries; skip if already present.
    let already = doc
        .get("workspace")
        .and_then(|i| i.as_array_of_tables())
        .map(|arr| {
            arr.iter().any(|t| {
                t.get("path")
                    .and_then(|v| v.as_str())
                    .map(|s| s == wanted)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if already {
        return Ok(());
    }

    if !doc.contains_key("workspace") {
        doc["workspace"] = toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    let arr = doc["workspace"]
        .as_array_of_tables_mut()
        .ok_or_else(|| anyhow!("global config has a non-array 'workspace' field"))?;
    let mut t = toml_edit::Table::new();
    t["path"] = toml_edit::value(wanted);
    arr.push(t);

    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&cfg_path, doc.to_string())
        .with_context(|| format!("writing {}", cfg_path.display()))?;
    Ok(())
}

/// Remove a workspace entry from the global registry by path.
/// Matches on the stored `path` string, with tolerance for `~` /
/// `${VAR}` expansion differences: both the stored value and the
/// input are resolved before compare. Silent no-op if the entry
/// isn't present. Preserves other fields / comments via toml_edit.
pub fn unregister_global_workspace(ws_path: &Path) -> Result<()> {
    let cfg_path = global_config_path()?;
    if !cfg_path.is_file() {
        return Ok(());
    }
    let existing = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("reading {}", cfg_path.display()))?;
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("parsing existing global config {}", cfg_path.display()))?;

    let canonical = ws_path
        .canonicalize()
        .unwrap_or_else(|_| ws_path.to_path_buf());
    let target = canonical.display().to_string();

    let Some(arr) = doc
        .get_mut("workspace")
        .and_then(|i| i.as_array_of_tables_mut())
    else {
        return Ok(());
    };
    let mut i = 0;
    while i < arr.len() {
        let matches_this = arr
            .get(i)
            .and_then(|t| t.get("path"))
            .and_then(|v| v.as_str())
            .and_then(|s| resolve_path(s, Path::new(".")).ok())
            .map(|p: PathBuf| p == canonical || p.display().to_string() == target)
            .unwrap_or(false);
        if matches_this {
            arr.remove(i);
        } else {
            i += 1;
        }
    }

    std::fs::write(&cfg_path, doc.to_string())
        .with_context(|| format!("writing {}", cfg_path.display()))?;
    Ok(())
}

/// List all workspace files registered globally, as absolute paths.
/// Paths that start with `~` or `${HOME}` are expanded. Missing
/// entries (files that no longer exist on disk) are filtered out so
/// the TUI doesn't render stale rows.
pub fn list_registered_workspaces() -> Result<Vec<PathBuf>> {
    let path = match global_config_path() {
        Ok(p) => p,
        Err(_) => return Ok(vec![]),
    };
    if !path.is_file() {
        return Ok(vec![]);
    }
    let global: GlobalFile = load_toml(&path)?;
    let mut out = Vec::with_capacity(global.workspaces.len());
    for entry in &global.workspaces {
        let expanded = resolve_path(&entry.path, std::path::Path::new("."))?;
        if expanded.is_file() {
            out.push(expanded);
        }
    }
    Ok(out)
}

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

use crate::domain::Workspace;

/// Inputs to [`load`]. All fields are optional and have sensible
/// defaults so that `LoadOptions::default()` + `load` does the obvious
/// thing: walk up from `$PWD`, pick up whatever global config exists.
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// Explicit workspace file path. If set, walk-up discovery is
    /// skipped. The path is loaded as-is.
    pub workspace_path: Option<PathBuf>,

    /// Starting directory for walk-up discovery. Defaults to the
    /// current process cwd at load time.
    pub cwd: Option<PathBuf>,

    /// Override for the global config path. Missing files at either
    /// the override or the default location are not an error — an
    /// empty [`GlobalFile`] is used.
    pub global_config_override: Option<PathBuf>,
}

/// Load the merged workspace for the current invocation.
///
/// Steps:
///   1. Locate the workspace file (explicit path > walk-up from cwd).
///   2. Load the global config (optional; defaults if missing).
///   3. For each project listed in the workspace, load its
///      `portagenty.toml` if present.
///   4. Merge the three tiers into a resolved [`Workspace`].
pub fn load(opts: &LoadOptions) -> Result<Workspace> {
    let ws_path = resolve_workspace_path(opts)?;
    let ws_file: WorkspaceFile = load_toml(&ws_path)
        .with_context(|| format!("loading workspace file {}", ws_path.display()))?;

    let global = load_global_file(opts)?;
    let per_project = load_per_project_files(&ws_file, &ws_path)?;

    merge::merge(&global, &ws_file, &ws_path, &per_project)
}

fn resolve_workspace_path(opts: &LoadOptions) -> Result<PathBuf> {
    if let Some(p) = &opts.workspace_path {
        return Ok(p.clone());
    }
    let cwd = match &opts.cwd {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("reading current directory")?,
    };
    walk_up_from(&cwd).ok_or_else(|| {
        anyhow!(
            "no *.portagenty.toml found walking up from {}",
            cwd.display()
        )
    })
}

fn load_global_file(opts: &LoadOptions) -> Result<GlobalFile> {
    let path = match &opts.global_config_override {
        Some(p) => p.clone(),
        None => match global_config_path() {
            Ok(p) => p,
            Err(_) => return Ok(GlobalFile::default()),
        },
    };
    if !path.is_file() {
        return Ok(GlobalFile::default());
    }
    load_toml(&path)
}

fn load_per_project_files(
    ws_file: &WorkspaceFile,
    ws_path: &Path,
) -> Result<merge::PerProjectFiles> {
    let ws_dir = ws_path.parent().unwrap_or_else(|| Path::new("."));
    let mut out = merge::PerProjectFiles::new();
    for raw in &ws_file.projects {
        let root = resolve_path(raw, ws_dir)?;
        if let Some(file) = project_file_in_dir(&root) {
            let pf: ProjectFile = load_toml(&file)?;
            out.insert(root, pf);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod default_mpx_tests {
    //! Round-trip + read tests for the global default-multiplexer
    //! helpers. Each test sandboxes XDG_CONFIG_HOME to a tempdir so
    //! the real user's config doesn't get touched. The tests are
    //! marked serial because they mutate process-wide env vars.
    use super::*;
    use crate::domain::Multiplexer;
    use serial_test::serial;

    /// Pin XDG_CONFIG_HOME to a fresh tempdir for the duration of
    /// the test; restore the previous value on Drop. Mirrors the
    /// pattern in `src/scaffold.rs`'s test module.
    struct TempXdg {
        _dir: assert_fs::TempDir,
        previous: Option<std::ffi::OsString>,
    }
    impl TempXdg {
        fn new() -> Self {
            let dir = assert_fs::TempDir::new().unwrap();
            let previous = std::env::var_os("XDG_CONFIG_HOME");
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            Self {
                _dir: dir,
                previous,
            }
        }
    }
    impl Drop for TempXdg {
        fn drop(&mut self) {
            match &self.previous {
                Some(p) => std::env::set_var("XDG_CONFIG_HOME", p),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    #[test]
    #[serial]
    fn current_default_returns_none_when_no_global_config() {
        let _xdg = TempXdg::new();
        assert_eq!(current_default_multiplexer().unwrap(), None);
    }

    #[test]
    #[serial]
    fn current_default_reads_zellij_back_after_set() {
        let _xdg = TempXdg::new();
        set_global_default_multiplexer(Multiplexer::Zellij).unwrap();
        assert_eq!(
            current_default_multiplexer().unwrap(),
            Some(Multiplexer::Zellij)
        );
    }

    #[test]
    #[serial]
    fn current_default_reads_tmux_back_after_set() {
        let _xdg = TempXdg::new();
        set_global_default_multiplexer(Multiplexer::Tmux).unwrap();
        assert_eq!(
            current_default_multiplexer().unwrap(),
            Some(Multiplexer::Tmux)
        );
    }

    #[test]
    #[serial]
    fn set_default_overwrites_previous_value() {
        let _xdg = TempXdg::new();
        set_global_default_multiplexer(Multiplexer::Tmux).unwrap();
        set_global_default_multiplexer(Multiplexer::Zellij).unwrap();
        assert_eq!(
            current_default_multiplexer().unwrap(),
            Some(Multiplexer::Zellij)
        );
    }

    #[test]
    #[serial]
    fn set_default_preserves_other_global_fields() {
        // Pre-seed the config with a [[workspace]] entry, then
        // verify set_global_default_multiplexer doesn't blow it
        // away — the toml_edit-based writer is supposed to preserve
        // unrelated content.
        let xdg = TempXdg::new();
        let cfg_dir = xdg._dir.path().join("portagenty");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg_file = cfg_dir.join("config.toml");
        std::fs::write(
            &cfg_file,
            "default-multiplexer = \"tmux\"\n\
             \n\
             [[workspace]]\n\
             path = \"/some/ws.portagenty.toml\"\n",
        )
        .unwrap();

        set_global_default_multiplexer(Multiplexer::Zellij).unwrap();

        let raw = std::fs::read_to_string(&cfg_file).unwrap();
        assert!(
            raw.contains("default-multiplexer = \"zellij\""),
            "default not updated: {raw}"
        );
        assert!(
            raw.contains("path = \"/some/ws.portagenty.toml\""),
            "workspace entry was lost: {raw}"
        );
    }

    #[test]
    #[serial]
    fn current_default_parses_zellij_from_kebab_case_field() {
        // Smoke test the wire format users actually see in their
        // config.toml — `default-multiplexer = "zellij"`. Catches a
        // regression where serde rename_all stops applying.
        let xdg = TempXdg::new();
        let cfg_dir = xdg._dir.path().join("portagenty");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            "default-multiplexer = \"zellij\"\n",
        )
        .unwrap();
        assert_eq!(
            current_default_multiplexer().unwrap(),
            Some(Multiplexer::Zellij)
        );
    }
}
