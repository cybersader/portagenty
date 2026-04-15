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
