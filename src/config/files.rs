//! On-disk TOML file shapes. These are the crate-private structures that
//! match what's actually written in `config.toml` / `*.portagenty.toml` /
//! `portagenty.toml`. Path strings here are raw: they may contain `~`,
//! `${VAR}`, or be relative to the file's directory.
//!
//! Path expansion + the three-tier merge happen in `config::merge`. The
//! types here are deliberately separate from `domain::*` so the on-disk
//! schema can evolve (e.g. new optional v1.x fields) without moving the
//! resolved domain types.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::domain::Multiplexer;

/// `$XDG_CONFIG_HOME/portagenty/config.toml` — machine-local registry
/// of known projects + known workspace files + default multiplexer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct GlobalFile {
    #[serde(default)]
    pub default_multiplexer: Option<Multiplexer>,

    /// Projects registered globally. Each entry has a raw path string
    /// (may start with `~` or `${VAR}`).
    #[serde(default, rename = "project")]
    pub projects: Vec<GlobalProjectEntry>,

    /// Known workspace files. Populates the TUI home screen.
    #[serde(default, rename = "workspace")]
    pub workspaces: Vec<GlobalWorkspaceEntry>,

    /// Machine-local TUI preferences (`[ui]` table). Additive; absent
    /// section deserializes to defaults.
    #[serde(default)]
    pub ui: UiSettings,
}

/// The `[ui]` table in the global config. Machine-local, not
/// committed. Holds preferences that shouldn't live in the
/// committable workspace TOML.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct UiSettings {
    /// When true, the TUI captures the mouse: click-to-select,
    /// double-click-to-open, scroll-wheel navigation. Off by default
    /// because capture disables the terminal's own click-drag text
    /// selection of the paths shown in rows. Toggled live with `m`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mouse: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct GlobalProjectEntry {
    pub path: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct GlobalWorkspaceEntry {
    pub path: String,
    /// Copy of the workspace TOML's `id` at registration time. Used
    /// to detect folder moves even when the old file is gone — a
    /// later registration of the same id at a different path means
    /// the workspace moved, and reconcile appends the old location
    /// to the workspace's `previous_paths`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// When true, this workspace is hidden from the picker's default
    /// list and only shown in the archived view (toggled with `A`).
    /// Machine-local view preference — archiving doesn't touch the
    /// workspace TOML or its on-disk file at all. Defaults to false;
    /// omitted from the wire when false to keep the config tidy.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
}

/// Any `*.portagenty.toml` with a non-empty prefix. The workspace
/// definition itself: name + mpx override + project list + sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct WorkspaceFile {
    pub name: String,

    /// Stable identity that survives folder moves and cross-env access.
    /// Auto-generated UUIDv4 on `pa init`; older files without one
    /// continue to work — the field is purely additive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Overrides the global default. `None` → inherit global.
    #[serde(default)]
    pub multiplexer: Option<Multiplexer>,

    /// Raw project path strings this workspace includes. Resolved at
    /// merge time against the workspace file's own directory.
    #[serde(default)]
    pub projects: Vec<String>,

    /// Free-form tags for organizing the workspace picker (filter /
    /// grouping perspectives). Committable — travels with the file.
    /// Additive; absent → empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    /// Historical on-disk locations this workspace has lived at
    /// before. Auto-appended by `pa` when walk-up detects the
    /// workspace's `id` was previously registered at a different
    /// path (i.e. the folder was moved). Consumed by portaconv to
    /// bridge to conversation histories that reference the old
    /// cwd. Snake-case on the wire — this is the one field that
    /// intentionally breaks with our kebab-case convention, so the
    /// portaconv contract stays stable and agent-friendly.
    #[serde(
        default,
        rename = "previous_paths",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub previous_paths: Vec<String>,

    #[serde(default, rename = "session")]
    pub sessions: Vec<RawSession>,
}

/// `portagenty.toml` at the root of a project directory. Minimal: only
/// session declarations. The project's identity is implicit from the
/// file's location.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct ProjectFile {
    #[serde(default, rename = "session")]
    pub sessions: Vec<RawSession>,
}

/// A session as written in a TOML file. `cwd` is raw — may be `~/foo`,
/// `${HOME}/foo`, `.`, `./foo`, or an absolute path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct RawSession {
    pub name: String,
    pub cwd: String,
    pub command: String,
    /// Optional `kind:` hint. Passes through to `domain::Session`
    /// verbatim; see that module for the enum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<crate::domain::SessionKind>,
    /// Per-session env vars. Passed through to the mpx adapter which
    /// decides how to apply them (tmux uses `-e KEY=VAL`; zellij
    /// wraps the command in `env KEY=VAL ...`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// Optional human-readable note describing the session. Carried
    /// through to `domain::Session` verbatim; display-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Read a TOML file and parse it into `T`. Preserves the path in the
/// error chain so the user sees which file was bad.
pub fn load_toml<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let contents =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str::<T>(&contents).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .prefix(name)
            .suffix(".toml")
            .tempfile()
            .unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    #[test]
    fn load_global_file_empty() {
        let f = write_tmp("global", "");
        let g: GlobalFile = load_toml(f.path()).unwrap();
        assert_eq!(g, GlobalFile::default());
    }

    #[test]
    fn load_global_file_full() {
        let src = r#"
default-multiplexer = "tmux"

[[project]]
path = "~/code/portagenty"
tags = ["rust", "agentic"]

[[project]]
path = "~/code/other"

[[workspace]]
path = "~/ws/agentic.portagenty.toml"
"#;
        let f = write_tmp("global-full", src);
        let g: GlobalFile = load_toml(f.path()).unwrap();
        assert_eq!(g.default_multiplexer, Some(Multiplexer::Tmux));
        assert_eq!(g.projects.len(), 2);
        assert_eq!(g.projects[0].tags, vec!["rust", "agentic"]);
        assert!(g.projects[1].tags.is_empty());
        assert_eq!(g.workspaces.len(), 1);
    }

    #[test]
    fn load_workspace_file() {
        let src = r#"
name = "Agentic stuff"
multiplexer = "tmux"
projects = ["~/code/portagenty", "./cyberbase"]

[[session]]
name = "claude"
cwd = "~/code/portagenty"
command = "claude"

[[session]]
name = "tests"
cwd = "."
command = "cargo nextest run"
"#;
        let f = write_tmp("ws", src);
        let w: WorkspaceFile = load_toml(f.path()).unwrap();
        assert_eq!(w.name, "Agentic stuff");
        assert_eq!(w.multiplexer, Some(Multiplexer::Tmux));
        assert_eq!(w.projects.len(), 2);
        assert_eq!(w.sessions.len(), 2);
        assert_eq!(w.sessions[0].cwd, "~/code/portagenty");
    }

    #[test]
    fn load_workspace_session_description() {
        let src = r#"
name = "ws"

[[session]]
name = "claude"
cwd = "."
command = "claude"
description = "main coding agent"
"#;
        let f = write_tmp("ws-desc", src);
        let w: WorkspaceFile = load_toml(f.path()).unwrap();
        assert_eq!(
            w.sessions[0].description.as_deref(),
            Some("main coding agent")
        );
    }

    #[test]
    fn description_absent_deserializes_as_none() {
        let src = r#"
name = "ws"

[[session]]
name = "claude"
cwd = "."
command = "claude"
"#;
        let f = write_tmp("ws-nodesc", src);
        let w: WorkspaceFile = load_toml(f.path()).unwrap();
        assert_eq!(w.sessions[0].description, None);
    }

    #[test]
    fn load_project_file() {
        let src = r#"
[[session]]
name = "dev"
cwd = "."
command = "bun run serve:dev"
"#;
        let f = write_tmp("proj", src);
        let p: ProjectFile = load_toml(f.path()).unwrap();
        assert_eq!(p.sessions.len(), 1);
        assert_eq!(p.sessions[0].name, "dev");
    }

    #[test]
    fn load_project_file_empty_has_no_sessions() {
        let f = write_tmp("proj-empty", "");
        let p: ProjectFile = load_toml(f.path()).unwrap();
        assert!(p.sessions.is_empty());
    }

    #[test]
    fn error_includes_path_on_missing_file() {
        let missing = std::path::PathBuf::from("/nonexistent/nowhere.toml");
        let err = load_toml::<GlobalFile>(&missing).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("/nonexistent/nowhere.toml"),
            "error missing file path: {msg}"
        );
    }

    #[test]
    fn error_includes_path_on_bad_toml() {
        let f = write_tmp("bad", "this is = not = valid");
        let err = load_toml::<GlobalFile>(f.path()).unwrap_err();
        let msg = format!("{err:#}");
        let expected = f.path().display().to_string();
        assert!(msg.contains(&expected), "error missing file path: {msg}");
    }
}
