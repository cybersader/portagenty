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

/// Write `contents` to `path` atomically: serialize into a sibling temp file,
/// fsync it, then `rename` onto the destination.
///
/// `rename(2)` within a directory is atomic, so a concurrent reader observes
/// either the whole old file or the whole new one — never a partial write.
/// This matters because several `pa` processes legitimately run at once (a
/// picker in one pane, `pa launch` in another, an agent session in a third)
/// and they all update the same global `config.toml`. With a plain
/// `fs::write`, a reader that opened the file mid-write got a truncated TOML
/// document and failed with a parse error such as `expected `.`, `=``.
///
/// A crash between write and rename leaves the original file intact; the
/// temp file is cleaned up on the error path. Mirrors `state::save_to`.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Unique per process so two concurrent writers never share a temp path.
    let tmp = path.with_extension(format!("toml.tmp.{}", std::process::id()));
    let write_result = (|| -> Result<()> {
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        // Durability is best-effort: a failed fsync shouldn't block the write.
        let _ = file.sync_all();
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write_result;
    }
    std::fs::rename(&tmp, path).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("renaming onto {}", path.display())
    })?;
    Ok(())
}

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
    write_atomic(&path, &doc.to_string())?;
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

    write_atomic(&cfg_path, &doc.to_string())?;
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

/// Ensure a file-backed workspace has the stable UUID required by supervised
/// launches. Legacy files without `id` are upgraded in place with toml_edit so
/// comments and session blocks survive. Existing IDs are validated and never
/// replaced; a malformed value remains an explicit, fail-closed error.
///
/// The workspace file is authoritative. After it is safely written, the global
/// registry entry is refreshed so protocol lookup and move reconciliation see
/// the same identity. Retrying after a registry-write failure is safe.
pub fn ensure_workspace_id(ws_path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(ws_path)
        .with_context(|| format!("reading workspace {}", ws_path.display()))?;
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing workspace {}", ws_path.display()))?;

    let id = match doc.get("id") {
        Some(item) => {
            let value = item.as_str().ok_or_else(|| {
                anyhow!(
                    "workspace id in {} must be a UUID string",
                    ws_path.display()
                )
            })?;
            uuid::Uuid::parse_str(value).with_context(|| {
                format!(
                    "workspace id {value:?} in {} is not a valid UUID",
                    ws_path.display()
                )
            })?;
            value.to_string()
        }
        None => {
            let generated = uuid::Uuid::new_v4().to_string();
            doc["id"] = toml_edit::value(generated.as_str());
            write_atomic(ws_path, &doc.to_string())?;
            generated
        }
    };

    register_global_workspace(ws_path).with_context(|| {
        format!(
            "workspace ID is valid, but refreshing its global registry entry failed for {}",
            ws_path.display()
        )
    })?;
    Ok(id)
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

    write_atomic(&cfg_path, &doc.to_string())?;
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
    write_atomic(&cfg_path, &cfg_doc.to_string())?;

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

/// Return the set of registered workspace paths flagged `archived =
/// true`, as canonicalized absolute paths. Missing-on-disk entries
/// are filtered out (same as [`list_registered_workspaces`]). The
/// picker uses this to partition its list into the default view vs
/// the archived view.
pub fn archived_workspaces() -> Result<std::collections::HashSet<PathBuf>> {
    let path = match global_config_path() {
        Ok(p) => p,
        Err(_) => return Ok(std::collections::HashSet::new()),
    };
    if !path.is_file() {
        return Ok(std::collections::HashSet::new());
    }
    let global: GlobalFile = load_toml(&path)?;
    let mut out = std::collections::HashSet::new();
    for entry in &global.workspaces {
        if !entry.archived {
            continue;
        }
        let expanded = resolve_path(&entry.path, std::path::Path::new("."))?;
        if expanded.is_file() {
            // Canonicalize so picker membership tests match regardless
            // of how the path was spelled at registration time.
            let canon = expanded.canonicalize().unwrap_or(expanded);
            out.insert(canon);
        }
    }
    Ok(out)
}

/// Read the machine-local `[ui] mouse` preference. `false` when the
/// config or the flag is absent (mouse is off by default).
pub fn ui_mouse_enabled() -> bool {
    let Ok(path) = global_config_path() else {
        return false;
    };
    if !path.is_file() {
        return false;
    }
    load_toml::<GlobalFile>(&path)
        .map(|g| g.ui.mouse)
        .unwrap_or(false)
}

/// Persist the `[ui] mouse` preference, preserving the rest of the
/// global config via toml_edit. Creates the file + parent dir if
/// needed. Best-effort — a write failure is surfaced to the caller.
pub fn set_ui_mouse(enabled: bool) -> Result<()> {
    let cfg_path = global_config_path()?;
    let existing = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("parsing existing global config {}", cfg_path.display()))?;
    if !doc.contains_key("ui") {
        doc["ui"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let ui = doc["ui"]
        .as_table_mut()
        .ok_or_else(|| anyhow!("global config has a non-table 'ui' field"))?;
    if enabled {
        ui["mouse"] = toml_edit::value(true);
    } else {
        // Clear the flag so the section stays tidy when off (and drop
        // an emptied `[ui]` table entirely).
        ui.remove("mouse");
        if ui.is_empty() {
            doc.remove("ui");
        }
    }
    write_atomic(&cfg_path, &doc.to_string())?;
    Ok(())
}

/// Set (or clear) the `archived` flag on a registered workspace,
/// matched by path. Idempotent; preserves all other fields/comments
/// via toml_edit. If the workspace isn't registered yet, it's added
/// with the requested flag so archiving a walk-up-only workspace
/// still sticks. Returns an error only on config I/O failure.
pub fn set_workspace_archived(ws_path: &Path, archived: bool) -> Result<()> {
    let cfg_path = global_config_path()?;
    let existing = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("parsing existing global config {}", cfg_path.display()))?;

    let canonical = ws_path
        .canonicalize()
        .unwrap_or_else(|_| ws_path.to_path_buf());
    let wanted = canonical.display().to_string();

    if !doc.contains_key("workspace") {
        doc["workspace"] = toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    let arr = doc["workspace"]
        .as_array_of_tables_mut()
        .ok_or_else(|| anyhow!("global config has a non-array 'workspace' field"))?;

    let mut found = false;
    for t in arr.iter_mut() {
        let matches_this = t
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| s == wanted)
            .unwrap_or(false);
        if matches_this {
            found = true;
            if archived {
                t["archived"] = toml_edit::value(true);
            } else {
                // Clear the flag entirely so unarchived rows go back
                // to the tidy `{ path, id? }` shape.
                t.remove("archived");
            }
            break;
        }
    }
    if !found {
        let mut t = toml_edit::Table::new();
        t["path"] = toml_edit::value(wanted);
        if let Some(id) = read_workspace_id(ws_path) {
            t["id"] = toml_edit::value(id.as_str());
        }
        if archived {
            t["archived"] = toml_edit::value(true);
        }
        arr.push(t);
    }

    write_atomic(&cfg_path, &doc.to_string())?;
    Ok(())
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
mod atomic_write_tests {
    use super::*;

    #[test]
    fn write_atomic_creates_parent_dirs_and_round_trips() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("deep/sub/config.toml");
        write_atomic(&path, "name = \"demo\"\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "name = \"demo\"\n");
    }

    #[test]
    fn write_atomic_leaves_no_temp_file_behind() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_atomic(&path, "a = 1\n").unwrap();
        let strays: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(strays.is_empty(), "left temp files: {strays:?}");
    }

    #[test]
    fn concurrent_writers_never_expose_a_partial_document() {
        // The bug this guards: config writes used a plain fs::write, so a
        // reader that opened the file mid-write saw a truncated TOML document
        // and failed with `expected `.`, `=``. Several `pa` processes updating
        // the global config at once is normal, so readers must only ever see a
        // complete document.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let tmp = assert_fs::TempDir::new().unwrap();
        let path = Arc::new(tmp.path().join("config.toml"));
        // Long, differently-sized payloads so a torn write would be obvious.
        let short = "default-multiplexer = \"tmux\"\n".to_string();
        let long = format!(
            "default-multiplexer = \"zellij\"\n{}",
            (0..400)
                .map(|i| format!("[[workspace]]\npath = \"/tmp/ws-{i}.portagenty.toml\"\n"))
                .collect::<String>()
        );
        write_atomic(&path, &short).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let path = Arc::clone(&path);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut torn = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let raw = std::fs::read_to_string(&*path).unwrap_or_default();
                    if raw.parse::<toml_edit::DocumentMut>().is_err() {
                        torn += 1;
                    }
                }
                torn
            })
        };

        for i in 0..200 {
            let body = if i % 2 == 0 { &short } else { &long };
            write_atomic(&path, body).unwrap();
        }
        stop.store(true, Ordering::Relaxed);

        let torn = reader.join().unwrap();
        assert_eq!(torn, 0, "reader observed {torn} partial documents");
    }
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
        _env: crate::test_env::EnvSandbox,
    }
    impl TempXdg {
        fn new() -> Self {
            let dir = assert_fs::TempDir::new().unwrap();
            let env = crate::test_env::EnvSandbox::new().set("XDG_CONFIG_HOME", dir.path());
            Self {
                _dir: dir,
                _env: env,
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
mod workspace_id_tests {
    use super::*;
    use serial_test::serial;
    use std::fs;

    struct Sandbox {
        _xdg: assert_fs::TempDir,
        _home: assert_fs::TempDir,
        _env: crate::test_env::EnvSandbox,
    }

    impl Sandbox {
        fn new() -> Self {
            let xdg = assert_fs::TempDir::new().unwrap();
            let home = assert_fs::TempDir::new().unwrap();
            let env = crate::test_env::EnvSandbox::new()
                .set("XDG_CONFIG_HOME", xdg.path())
                .set("HOME", home.path());
            Self {
                _xdg: xdg,
                _home: home,
                _env: env,
            }
        }
    }

    fn legacy_workspace(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("legacy.portagenty.toml");
        fs::write(
            &path,
            "# keep this comment\nname = \"legacy\"\nmultiplexer = \"tmux\"\n\n[[session]]\nname = \"shell\"\ncwd = \".\"\ncommand = \"bash\"\n",
        )
        .unwrap();
        path
    }

    #[test]
    #[serial]
    fn ensure_workspace_id_upgrades_legacy_file_and_registry_idempotently() {
        let _sandbox = Sandbox::new();
        let temp = assert_fs::TempDir::new().unwrap();
        let path = legacy_workspace(temp.path());

        let id = ensure_workspace_id(&path).unwrap();
        uuid::Uuid::parse_str(&id).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        assert!(
            first.contains("# keep this comment"),
            "comment lost: {first}"
        );
        assert!(first.contains("[[session]]"), "session lost: {first}");
        assert!(
            first.contains(&format!("id = \"{id}\"")),
            "id missing: {first}"
        );
        let registry = fs::read_to_string(global_config_path().unwrap()).unwrap();
        assert!(registry.contains(&id), "registry ID missing: {registry}");

        assert_eq!(ensure_workspace_id(&path).unwrap(), id);
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
    }

    #[test]
    #[serial]
    fn ensure_workspace_id_refuses_invalid_existing_value_without_rewrite() {
        let _sandbox = Sandbox::new();
        let temp = assert_fs::TempDir::new().unwrap();
        let path = temp.path().join("bad.portagenty.toml");
        let raw = "name = \"bad\"\nid = \"not-a-uuid\"\nmultiplexer = \"tmux\"\n";
        fs::write(&path, raw).unwrap();

        let error = ensure_workspace_id(&path).unwrap_err();
        assert!(format!("{error:#}").contains("not a valid UUID"));
        assert_eq!(fs::read_to_string(&path).unwrap(), raw);
        assert!(!global_config_path().unwrap().exists());
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn ensure_workspace_id_write_failure_leaves_legacy_file_unchanged() {
        use std::os::unix::fs::PermissionsExt;

        let _sandbox = Sandbox::new();
        let temp = assert_fs::TempDir::new().unwrap();
        let path = legacy_workspace(temp.path());
        let before = fs::read_to_string(&path).unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o555)).unwrap();

        let result = ensure_workspace_id(&path);

        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            result.is_err(),
            "read-only directory unexpectedly accepted a write"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
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
        _env: crate::test_env::EnvSandbox,
    }
    impl Sandbox {
        fn new() -> Self {
            let xdg = assert_fs::TempDir::new().unwrap();
            let home = assert_fs::TempDir::new().unwrap();
            let env = crate::test_env::EnvSandbox::new()
                .set("XDG_CONFIG_HOME", xdg.path())
                .set("HOME", home.path());
            Self {
                _xdg: xdg,
                _home: home,
                _env: env,
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
        let workspace: WorkspaceFile = load_toml(&new_p).unwrap();
        assert_eq!(
            workspace.previous_paths.len(),
            1,
            "previous_paths duplicated: {workspace:?}"
        );
        let recorded = PathBuf::from(&workspace.previous_paths[0]);
        let recorded_canonical = recorded.canonicalize().unwrap_or_else(|_| recorded.clone());
        let expected_canonical = old_tmp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| old_tmp.path().to_path_buf());
        assert_eq!(recorded_canonical, expected_canonical);
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
            .filter(|rp| rp.canonicalize().ok() == p.canonicalize().ok())
            .collect();
        assert_eq!(matches.len(), 1, "row count drifted: {regged:?}");
    }

    #[test]
    #[serial]
    fn ui_mouse_round_trips_and_stays_tidy_when_off() {
        let _s = Sandbox::new();
        assert!(!ui_mouse_enabled(), "default should be off");
        set_ui_mouse(true).unwrap();
        assert!(ui_mouse_enabled(), "enable didn't stick");
        let cfg = fs::read_to_string(global_config_path().unwrap()).unwrap();
        assert!(cfg.contains("[ui]") && cfg.contains("mouse"), "cfg: {cfg}");
        set_ui_mouse(false).unwrap();
        assert!(!ui_mouse_enabled(), "disable didn't stick");
        let cfg = fs::read_to_string(global_config_path().unwrap()).unwrap();
        // Emptied [ui] table is dropped so the file stays tidy.
        assert!(!cfg.contains("mouse"), "mouse key lingered: {cfg}");
    }

    #[test]
    #[serial]
    fn archive_then_unarchive_round_trips() {
        let _s = Sandbox::new();
        let tmp = assert_fs::TempDir::new().unwrap();
        let p = write_ws(tmp.path(), "abababab-cdcd-efef-0101-202020202020");
        register_global_workspace(&p).unwrap();

        // Not archived initially.
        assert!(
            archived_workspaces().unwrap().is_empty(),
            "fresh registration should not be archived"
        );

        set_workspace_archived(&p, true).unwrap();
        let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
        assert!(
            archived_workspaces().unwrap().contains(&canon),
            "archive flag not reflected in archived_workspaces()"
        );
        // list_registered_workspaces still includes archived (so
        // resolve-by-id / protocol can still open them).
        assert!(
            list_registered_workspaces().unwrap().iter().any(|rp| rp
                .canonicalize()
                .ok()
                .as_deref()
                == Some(&canon)),
            "archived workspace dropped from full registry list"
        );

        set_workspace_archived(&p, false).unwrap();
        assert!(
            archived_workspaces().unwrap().is_empty(),
            "unarchive didn't clear the flag"
        );
        // Unarchived row should be back to the tidy shape (no
        // `archived` key lingering).
        let cfg_raw = fs::read_to_string(global_config_path().unwrap()).unwrap();
        assert!(
            !cfg_raw.contains("archived"),
            "archived key not removed on unarchive: {cfg_raw}"
        );
    }

    #[test]
    #[serial]
    fn archive_is_idempotent_and_keeps_single_row() {
        let _s = Sandbox::new();
        let tmp = assert_fs::TempDir::new().unwrap();
        let p = write_ws(tmp.path(), "12121212-3434-5656-7878-909090909090");
        register_global_workspace(&p).unwrap();
        set_workspace_archived(&p, true).unwrap();
        set_workspace_archived(&p, true).unwrap();
        let regged = list_registered_workspaces().unwrap();
        let matches: Vec<_> = regged
            .iter()
            .filter(|rp| rp.canonicalize().ok() == p.canonicalize().ok())
            .collect();
        assert_eq!(matches.len(), 1, "archive duplicated the row: {regged:?}");
    }

    #[test]
    #[serial]
    fn archiving_unregistered_workspace_adds_a_row() {
        let _s = Sandbox::new();
        let tmp = assert_fs::TempDir::new().unwrap();
        // Note: NOT registered first — archiving a walk-up-only
        // workspace should still create the entry so it sticks.
        let p = write_ws(tmp.path(), "fefefefe-0000-1111-2222-333344445555");
        set_workspace_archived(&p, true).unwrap();
        let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
        assert!(
            archived_workspaces().unwrap().contains(&canon),
            "archiving an unregistered workspace didn't persist"
        );
    }
}
