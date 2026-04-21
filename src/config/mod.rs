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
///
/// If the workspace file has an `id` field, it's mirrored into the
/// registry entry so the reconcile step
/// ([`reconcile_previous_paths_on_reregister`]) can match folder
/// moves even after the old file has been deleted. Re-registering a
/// path whose `id` changed refreshes the stored `id` without
/// duplicating the row.
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

    let ws_id = read_workspace_id(ws_path);

    if !doc.contains_key("workspace") {
        doc["workspace"] = toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    let arr = doc["workspace"]
        .as_array_of_tables_mut()
        .ok_or_else(|| anyhow!("global config has a non-array 'workspace' field"))?;

    // Find an existing row at the same path. If found, refresh the
    // mirrored id (it may have been added after the initial
    // registration). Otherwise append a new row.
    let mut existing_idx: Option<usize> = None;
    for (idx, t) in arr.iter().enumerate() {
        if t.get("path")
            .and_then(|v| v.as_str())
            .map(|s| s == wanted)
            .unwrap_or(false)
        {
            existing_idx = Some(idx);
            break;
        }
    }

    match existing_idx {
        Some(idx) => {
            let row = arr.get_mut(idx).expect("idx just observed");
            match &ws_id {
                Some(id) => row["id"] = toml_edit::value(id.as_str()),
                None => {
                    row.remove("id");
                }
            }
        }
        None => {
            let mut t = toml_edit::Table::new();
            t["path"] = toml_edit::value(wanted);
            if let Some(id) = &ws_id {
                t["id"] = toml_edit::value(id.as_str());
            }
            arr.push(t);
        }
    }

    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&cfg_path, doc.to_string())
        .with_context(|| format!("writing {}", cfg_path.display()))?;
    Ok(())
}

/// Best-effort read of the `id` field from a workspace TOML. Returns
/// `None` when the file is unreadable, unparseable, or has no `id` —
/// all of which are OK (ids are purely additive; legacy files work).
fn read_workspace_id(ws_path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(ws_path).ok()?;
    let doc: toml_edit::DocumentMut = raw.parse().ok()?;
    doc.get("id").and_then(|v| v.as_str()).map(str::to_string)
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

/// Detect whether walk-up just re-registered a workspace at a new
/// on-disk location, and if so append the old location(s) to its
/// `previous_paths`. External tools (portaconv) read that field to
/// bridge to conversation histories authored when the project lived
/// at the old path — without it, moving a folder silently orphans
/// prior Claude Code sessions keyed by the old cwd.
///
/// Trigger: the workspace's TOML has an `id` that's recorded in one
/// or more global-registry entries at a *different* canonical path.
/// Those stale paths become the `previous_paths` additions (stored
/// as the workspace file's parent directory — portaconv matches
/// JSONL `cwd` prefixes against directories, not TOML files).
///
/// Silent no-op when:
///   - the workspace file has no `id`,
///   - no matching stale registry entry is found,
///   - the previous directory is already listed.
///
/// Returns the list of old directories that were newly recorded.
/// Stale registry entries for the same id at different paths are
/// dropped (registry = current location only; history lives in the
/// committed TOML). Errors are only raised for filesystem I/O on the
/// files we're writing — unparseable side-files are skipped.
pub fn reconcile_previous_paths_on_reregister(new_path: &Path) -> Result<Vec<PathBuf>> {
    let Some(new_id) = read_workspace_id(new_path) else {
        return Ok(vec![]);
    };
    let new_canonical = new_path
        .canonicalize()
        .unwrap_or_else(|_| new_path.to_path_buf());

    let cfg_path = global_config_path()?;
    if !cfg_path.is_file() {
        return Ok(vec![]);
    }
    let cfg_raw = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("reading {}", cfg_path.display()))?;
    let mut cfg_doc: toml_edit::DocumentMut = cfg_raw
        .parse()
        .with_context(|| format!("parsing {}", cfg_path.display()))?;

    // Gather matching stale entries. Iterate by index so we can mutate
    // cfg_doc in a second pass without borrowck pain.
    let mut stale_indices: Vec<usize> = Vec::new();
    let mut old_paths: Vec<PathBuf> = Vec::new();
    if let Some(arr) = cfg_doc
        .get("workspace")
        .and_then(|v| v.as_array_of_tables())
    {
        for (idx, table) in arr.iter().enumerate() {
            let Some(entry_path_str) = table.get("path").and_then(|v| v.as_str()) else {
                continue;
            };
            let entry_path = match resolve_path(entry_path_str, Path::new(".")) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let entry_canonical = entry_path
                .canonicalize()
                .unwrap_or_else(|_| entry_path.clone());
            if entry_canonical == new_canonical {
                continue;
            }
            let stored_id = table.get("id").and_then(|v| v.as_str());
            let id_matches = if stored_id == Some(new_id.as_str()) {
                true
            } else if entry_path.is_file() {
                // Fallback for registry entries written before the id
                // mirror existed: look up the old file's id.
                read_workspace_id(&entry_path).as_deref() == Some(new_id.as_str())
            } else {
                false
            };
            if id_matches {
                stale_indices.push(idx);
                old_paths.push(entry_path);
            }
        }
    }

    if old_paths.is_empty() {
        return Ok(vec![]);
    }

    // Map old TOML file paths → their parent directories. That's the
    // on-disk location portaconv matches against: Claude Code JSONLs
    // are keyed by cwd (the directory), never by the workspace file
    // path.
    let old_dirs: Vec<PathBuf> = old_paths
        .iter()
        .map(|p| {
            p.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| p.clone())
        })
        .collect();

    let newly_added = append_previous_paths(new_path, &old_dirs)?;

    // Drop stale registry entries. Walk indices in reverse so earlier
    // indices stay valid as we remove.
    if let Some(arr) = cfg_doc
        .get_mut("workspace")
        .and_then(|v| v.as_array_of_tables_mut())
    {
        for idx in stale_indices.into_iter().rev() {
            arr.remove(idx);
        }
    }
    std::fs::write(&cfg_path, cfg_doc.to_string())
        .with_context(|| format!("writing {}", cfg_path.display()))?;

    Ok(newly_added)
}

/// Append each old directory to the workspace TOML's `previous_paths`
/// array (creating it if absent), de-duped against existing entries.
/// Preserves all other content via toml_edit. Returns the directories
/// that were newly added — an empty vec means everything was already
/// listed.
fn append_previous_paths(ws_path: &Path, old_dirs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let raw = std::fs::read_to_string(ws_path)
        .with_context(|| format!("reading {}", ws_path.display()))?;
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing {}", ws_path.display()))?;

    let mut existing: Vec<String> = doc
        .get("previous_paths")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let mut newly: Vec<PathBuf> = Vec::new();
    for dir in old_dirs {
        let s = dir.display().to_string();
        if !existing.iter().any(|e| e == &s) {
            existing.push(s);
            newly.push(dir.clone());
        }
    }
    if newly.is_empty() {
        return Ok(vec![]);
    }

    let mut arr = toml_edit::Array::new();
    for s in &existing {
        arr.push(s.as_str());
    }
    doc["previous_paths"] = toml_edit::value(arr);

    std::fs::write(ws_path, doc.to_string())
        .with_context(|| format!("writing {}", ws_path.display()))?;
    Ok(newly)
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

#[cfg(test)]
mod previous_paths_tests {
    //! Reconcile-on-re-register + registry-id tracking tests. Each
    //! test sandboxes XDG_CONFIG_HOME + HOME so real user config /
    //! conversation state is never touched. Tests are serial because
    //! they mutate process-wide env.
    use super::*;
    use serial_test::serial;
    use std::fs;

    struct Sandbox {
        _xdg: assert_fs::TempDir,
        _home: assert_fs::TempDir,
        prev_xdg: Option<std::ffi::OsString>,
        prev_home: Option<std::ffi::OsString>,
    }
    impl Sandbox {
        fn new() -> Self {
            let xdg = assert_fs::TempDir::new().unwrap();
            let home = assert_fs::TempDir::new().unwrap();
            let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
            let prev_home = std::env::var_os("HOME");
            std::env::set_var("XDG_CONFIG_HOME", xdg.path());
            std::env::set_var("HOME", home.path());
            Self {
                _xdg: xdg,
                _home: home,
                prev_xdg,
                prev_home,
            }
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            match &self.prev_xdg {
                Some(p) => std::env::set_var("XDG_CONFIG_HOME", p),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match &self.prev_home {
                Some(p) => std::env::set_var("HOME", p),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn write_ws(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
        fs::create_dir_all(dir).unwrap();
        let p = dir.join("demo.portagenty.toml");
        fs::write(
            &p,
            format!(
                "name = \"demo\"\nid = \"{id}\"\nmultiplexer = \"tmux\"\n\n[[session]]\nname = \"shell\"\ncwd = \".\"\ncommand = \"bash\"\n"
            ),
        )
        .unwrap();
        p
    }

    #[test]
    #[serial]
    fn no_op_when_workspace_has_no_id() {
        let _s = Sandbox::new();
        let tmp = assert_fs::TempDir::new().unwrap();
        let p = tmp.path().join("demo.portagenty.toml");
        fs::write(&p, "name = \"demo\"\nmultiplexer = \"tmux\"\n").unwrap();
        register_global_workspace(&p).unwrap();
        let added = reconcile_previous_paths_on_reregister(&p).unwrap();
        assert!(added.is_empty(), "no id → no reconcile: {added:?}");
    }

    #[test]
    #[serial]
    fn no_op_on_first_registration() {
        let _s = Sandbox::new();
        let tmp = assert_fs::TempDir::new().unwrap();
        let p = write_ws(tmp.path(), "aaaa1111-bbbb-cccc-dddd-eeee22223333");
        register_global_workspace(&p).unwrap();
        let added = reconcile_previous_paths_on_reregister(&p).unwrap();
        assert!(added.is_empty(), "first reg → no history: {added:?}");
        let raw = fs::read_to_string(&p).unwrap();
        assert!(
            !raw.contains("previous_paths"),
            "first registration shouldn't touch previous_paths: {raw}"
        );
    }

    #[test]
    #[serial]
    fn records_old_path_when_workspace_moves_while_old_file_exists() {
        let _s = Sandbox::new();
        // Simulate: user copies (not deletes) the workspace folder —
        // both files exist. Newly-registered path should pick up the
        // old path as a `previous_paths` entry, and the stale registry
        // entry should be dropped.
        let id = "11111111-2222-3333-4444-555555555555";
        let old_tmp = assert_fs::TempDir::new().unwrap();
        let new_tmp = assert_fs::TempDir::new().unwrap();
        let old_p = write_ws(old_tmp.path(), id);
        let new_p = write_ws(new_tmp.path(), id);

        register_global_workspace(&old_p).unwrap();
        register_global_workspace(&new_p).unwrap();

        let added = reconcile_previous_paths_on_reregister(&new_p).unwrap();
        assert_eq!(added.len(), 1, "expected one old dir, got {added:?}");

        let new_raw = fs::read_to_string(&new_p).unwrap();
        let old_dir_canonical = old_tmp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| old_tmp.path().to_path_buf());
        assert!(
            new_raw.contains("previous_paths"),
            "previous_paths absent after reconcile: {new_raw}"
        );
        assert!(
            new_raw.contains(&old_dir_canonical.display().to_string())
                || new_raw.contains(&old_tmp.path().display().to_string()),
            "old dir missing from previous_paths: {new_raw}"
        );

        // Registry: stale entry dropped.
        let regged = list_registered_workspaces().unwrap();
        let has_old = regged
            .iter()
            .any(|p| p.canonicalize().ok().as_deref() == Some(&old_p));
        assert!(!has_old, "stale registry entry not dropped: {regged:?}");
    }

    #[test]
    #[serial]
    fn records_old_path_via_registry_id_when_old_file_is_gone() {
        let _s = Sandbox::new();
        // The realistic move: user `mv`s the folder, old file is
        // gone. The registry's mirrored id is the only remaining
        // evidence of the old location — reconcile uses it to bridge.
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let old_tmp = assert_fs::TempDir::new().unwrap();
        let old_p = write_ws(old_tmp.path(), id);
        register_global_workspace(&old_p).unwrap();
        // Nuke the old file to simulate `mv`.
        fs::remove_file(&old_p).unwrap();

        let new_tmp = assert_fs::TempDir::new().unwrap();
        let new_p = write_ws(new_tmp.path(), id);
        register_global_workspace(&new_p).unwrap();

        let added = reconcile_previous_paths_on_reregister(&new_p).unwrap();
        assert_eq!(
            added.len(),
            1,
            "expected one recovered old dir, got {added:?}"
        );
        let new_raw = fs::read_to_string(&new_p).unwrap();
        assert!(
            new_raw.contains(&old_tmp.path().display().to_string())
                || new_raw.contains(
                    &old_tmp
                        .path()
                        .canonicalize()
                        .unwrap_or_else(|_| old_tmp.path().to_path_buf())
                        .display()
                        .to_string()
                ),
            "old dir missing from previous_paths: {new_raw}"
        );
    }

    #[test]
    #[serial]
    fn dedupes_previous_paths_on_repeated_reconcile() {
        let _s = Sandbox::new();
        let id = "cccccccc-dddd-eeee-ffff-000011112222";
        let old_tmp = assert_fs::TempDir::new().unwrap();
        let new_tmp = assert_fs::TempDir::new().unwrap();
        write_ws(old_tmp.path(), id);
        let new_p = write_ws(new_tmp.path(), id);

        register_global_workspace(&old_tmp.path().join("demo.portagenty.toml")).unwrap();
        register_global_workspace(&new_p).unwrap();

        reconcile_previous_paths_on_reregister(&new_p).unwrap();
        // Re-run should not grow the array or fail.
        let second = reconcile_previous_paths_on_reregister(&new_p).unwrap();
        assert!(
            second.is_empty(),
            "second pass should be a no-op: {second:?}"
        );
        let new_raw = fs::read_to_string(&new_p).unwrap();
        // Count occurrences of the old path — exactly one should be present.
        let old_s = old_tmp.path().display().to_string();
        let old_canonical = old_tmp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| old_tmp.path().to_path_buf())
            .display()
            .to_string();
        let hits = new_raw.matches(old_s.as_str()).count()
            + if old_s == old_canonical {
                0
            } else {
                new_raw.matches(old_canonical.as_str()).count()
            };
        assert_eq!(hits, 1, "previous_paths duplicated: {new_raw}");
    }

    #[test]
    #[serial]
    fn register_stores_id_from_toml_in_registry_entry() {
        let _s = Sandbox::new();
        let tmp = assert_fs::TempDir::new().unwrap();
        let p = write_ws(tmp.path(), "deadbeef-0000-1111-2222-333344445555");
        register_global_workspace(&p).unwrap();
        let cfg_raw = fs::read_to_string(global_config_path().unwrap()).unwrap();
        assert!(
            cfg_raw.contains("deadbeef-0000-1111-2222-333344445555"),
            "registry entry missing mirrored id: {cfg_raw}"
        );
    }

    #[test]
    #[serial]
    fn re_registering_same_path_with_new_id_updates_in_place() {
        let _s = Sandbox::new();
        let tmp = assert_fs::TempDir::new().unwrap();
        let p = write_ws(tmp.path(), "11111111-1111-1111-1111-111111111111");
        register_global_workspace(&p).unwrap();
        // Rewrite the file with a new id; re-register same path.
        fs::write(
            &p,
            "name = \"demo\"\nid = \"22222222-2222-2222-2222-222222222222\"\nmultiplexer = \"tmux\"\n",
        )
        .unwrap();
        register_global_workspace(&p).unwrap();
        let cfg_raw = fs::read_to_string(global_config_path().unwrap()).unwrap();
        assert!(
            !cfg_raw.contains("11111111-1111-1111-1111-111111111111"),
            "old id still present after refresh: {cfg_raw}"
        );
        assert!(
            cfg_raw.contains("22222222-2222-2222-2222-222222222222"),
            "new id not written: {cfg_raw}"
        );
        // And the workspace row count is still one.
        let regged = list_registered_workspaces().unwrap();
        let matches: Vec<_> = regged
            .iter()
            .filter(|rp| rp.canonicalize().ok().as_deref() == Some(&p))
            .collect();
        assert_eq!(matches.len(), 1, "row count drifted: {regged:?}");
    }
}
