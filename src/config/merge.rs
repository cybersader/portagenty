//! Three-tier merge: turn raw on-disk file contents into a
//! `domain::Workspace`. See `DESIGN.md` §2.
//!
//! Path resolution lives here too: `~`, `${VAR}`, and relative-to-file
//! paths all become absolute before a `Workspace` leaves this module,
//! so every downstream consumer (mux, tui, cli) sees absolute paths only.

use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::files::{GlobalFile, ProjectFile, RawSession, WorkspaceFile};
use crate::domain::{Session, Workspace};

/// Expand `~` and `${VAR}` and return the resulting string. Does not
/// resolve relative paths — that's the caller's job.
pub fn expand(raw: &str) -> Result<String> {
    let tilded = expand_tilde(raw)?;
    expand_vars(&tilded)
}

fn home_dir() -> Result<String> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| anyhow!("neither HOME nor USERPROFILE is set; cannot expand '~'"))
}

fn expand_tilde(raw: &str) -> Result<String> {
    if raw == "~" {
        home_dir()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        Ok(format!("{}/{}", home_dir()?, rest))
    } else {
        Ok(raw.to_string())
    }
}

fn expand_vars(raw: &str) -> Result<String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_name = String::new();
            let mut closed = false;
            for nc in chars.by_ref() {
                if nc == '}' {
                    closed = true;
                    break;
                }
                var_name.push(nc);
            }
            if !closed {
                return Err(anyhow!("unterminated ${{...}} in {raw:?}"));
            }
            let val = std::env::var(&var_name)
                .map_err(|_| anyhow!("env var ${{{var_name}}} is not set"))?;
            out.push_str(&val);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// Resolve a raw path string against `base`, expanding `~`/`${VAR}` and
/// joining relative paths onto `base`.
pub fn resolve_path(raw: &str, base: &Path) -> Result<PathBuf> {
    let expanded = PathBuf::from(expand(raw)?);
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(base.join(expanded))
    }
}

fn to_session(rs: &RawSession, cwd_base: &Path) -> Result<Session> {
    Ok(Session {
        name: rs.name.clone(),
        cwd: resolve_path(&rs.cwd, cwd_base)
            .with_context(|| format!("resolving cwd for session {:?}", rs.name))?,
        command: rs.command.clone(),
        kind: rs.kind,
        env: rs.env.clone(),
    })
}

/// Per-project files the merge needs, keyed by resolved project root.
/// `BTreeMap` so ordering is deterministic across runs — tests snapshot
/// this through the resulting `Workspace.sessions`.
pub type PerProjectFiles = BTreeMap<PathBuf, ProjectFile>;

/// Combine the three tiers into a resolved `Workspace`.
///
/// Session precedence on a name collision: workspace beats per-project
/// beats global. v1 has no global-level sessions, so the effective rule
/// is "workspace wins, then per-project." Insertion order is preserved
/// within each tier.
pub fn merge(
    global: &GlobalFile,
    workspace_file: &WorkspaceFile,
    workspace_file_path: &Path,
    per_project: &PerProjectFiles,
) -> Result<Workspace> {
    let ws_dir = workspace_file_path
        .parent()
        .unwrap_or_else(|| Path::new("."));

    let mut projects = Vec::with_capacity(workspace_file.projects.len());
    for raw in &workspace_file.projects {
        projects.push(
            resolve_path(raw, ws_dir).with_context(|| format!("resolving project path {raw:?}"))?,
        );
    }

    let mut sessions: Vec<Session> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Tier 1: workspace-level sessions. Cwds resolve relative to the
    // workspace file's directory.
    for rs in &workspace_file.sessions {
        if seen.insert(rs.name.clone()) {
            sessions.push(to_session(rs, ws_dir)?);
        }
    }

    // Tier 2: per-project sessions. Cwds resolve relative to the
    // project root (the directory containing the `portagenty.toml`).
    for (proj_root, pf) in per_project {
        for rs in &pf.sessions {
            if seen.insert(rs.name.clone()) {
                sessions.push(to_session(rs, proj_root)?);
            }
        }
    }

    // Tier 3 (global): v1 has no global sessions.

    let multiplexer = workspace_file
        .multiplexer
        .or(global.default_multiplexer)
        .unwrap_or_default();

    Ok(Workspace {
        name: workspace_file.name.clone(),
        id: workspace_file.id.clone(),
        file_path: Some(workspace_file_path.to_path_buf()),
        multiplexer,
        projects,
        sessions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::files::{GlobalFile, RawSession, WorkspaceFile};
    use crate::domain::Multiplexer;

    fn session(name: &str, cwd: &str, cmd: &str) -> RawSession {
        RawSession {
            name: name.into(),
            cwd: cwd.into(),
            command: cmd.into(),
            kind: None,
            env: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn resolve_path_expands_tilde_and_joins_relative() {
        std::env::set_var("HOME", "/home/test");
        let out = resolve_path("~/foo", Path::new("/irrelevant")).unwrap();
        assert_eq!(out, PathBuf::from("/home/test/foo"));

        let out = resolve_path("./bar", Path::new("/base/dir")).unwrap();
        assert_eq!(out, PathBuf::from("/base/dir/./bar"));

        let out = resolve_path("/absolute/p", Path::new("/ignored")).unwrap();
        assert_eq!(out, PathBuf::from("/absolute/p"));
    }

    #[test]
    fn resolve_path_expands_braced_vars() {
        std::env::set_var("PA_TEST_VAR", "/x");
        let out = resolve_path("${PA_TEST_VAR}/y", Path::new("/i")).unwrap();
        assert_eq!(out, PathBuf::from("/x/y"));
    }

    #[test]
    fn resolve_path_errors_on_unset_var() {
        std::env::remove_var("PA_NOT_SET_XYZ");
        let err = resolve_path("${PA_NOT_SET_XYZ}/y", Path::new("/b")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("PA_NOT_SET_XYZ"), "{msg}");
    }

    #[test]
    fn merge_workspace_only() {
        let global = GlobalFile::default();
        let ws_path = PathBuf::from("/ws/example.portagenty.toml");
        let wf = WorkspaceFile {
            name: "ex".into(),
            id: None,
            multiplexer: None,
            projects: vec!["/p".into()],
            previous_paths: vec![],
            sessions: vec![session("s1", "/abs", "echo 1")],
        };
        let per_project = PerProjectFiles::new();
        let w = merge(&global, &wf, &ws_path, &per_project).unwrap();
        assert_eq!(w.name, "ex");
        assert_eq!(w.multiplexer, Multiplexer::Tmux);
        assert_eq!(w.projects, vec![PathBuf::from("/p")]);
        assert_eq!(w.sessions.len(), 1);
        assert_eq!(w.sessions[0].cwd, PathBuf::from("/abs"));
    }

    #[test]
    fn merge_workspace_overrides_per_project_on_name_collision() {
        let global = GlobalFile::default();
        let ws_path = PathBuf::from("/ws/example.portagenty.toml");
        let wf = WorkspaceFile {
            name: "ex".into(),
            id: None,
            multiplexer: None,
            projects: vec!["/p".into()],
            previous_paths: vec![],
            sessions: vec![session("shared", "/abs", "workspace-version")],
        };
        let mut per_project = PerProjectFiles::new();
        per_project.insert(
            PathBuf::from("/p"),
            ProjectFile {
                sessions: vec![
                    session("shared", ".", "project-version"),
                    session("extra", ".", "only-in-project"),
                ],
            },
        );

        let w = merge(&global, &wf, &ws_path, &per_project).unwrap();
        // `shared` comes from the workspace tier; `extra` from project.
        assert_eq!(w.sessions.len(), 2);
        let shared = w.sessions.iter().find(|s| s.name == "shared").unwrap();
        assert_eq!(shared.command, "workspace-version");
        let extra = w.sessions.iter().find(|s| s.name == "extra").unwrap();
        assert_eq!(extra.command, "only-in-project");
    }

    #[test]
    fn merge_multiplexer_precedence() {
        let ws_path = PathBuf::from("/ws/example.portagenty.toml");
        let wf_empty = WorkspaceFile {
            name: "ex".into(),
            id: None,
            multiplexer: None,
            projects: vec![],
            previous_paths: vec![],
            sessions: vec![],
        };

        // Workspace override wins over global default.
        let global_zj = GlobalFile {
            default_multiplexer: Some(Multiplexer::Zellij),
            ..Default::default()
        };
        let wf_tmux = WorkspaceFile {
            multiplexer: Some(Multiplexer::Tmux),
            ..wf_empty.clone()
        };
        let w = merge(&global_zj, &wf_tmux, &ws_path, &PerProjectFiles::new()).unwrap();
        assert_eq!(w.multiplexer, Multiplexer::Tmux);

        // Global default is used when workspace doesn't override.
        let w = merge(&global_zj, &wf_empty, &ws_path, &PerProjectFiles::new()).unwrap();
        assert_eq!(w.multiplexer, Multiplexer::Zellij);

        // Default-of-default is Tmux when nothing specifies.
        let w = merge(
            &GlobalFile::default(),
            &wf_empty,
            &ws_path,
            &PerProjectFiles::new(),
        )
        .unwrap();
        assert_eq!(w.multiplexer, Multiplexer::Tmux);
    }

    #[test]
    fn per_project_cwds_resolve_relative_to_project_root() {
        let global = GlobalFile::default();
        let ws_path = PathBuf::from("/ws/example.portagenty.toml");
        let wf = WorkspaceFile {
            name: "ex".into(),
            id: None,
            multiplexer: None,
            projects: vec!["/real/project".into()],
            previous_paths: vec![],
            sessions: vec![],
        };
        let mut per_project = PerProjectFiles::new();
        per_project.insert(
            PathBuf::from("/real/project"),
            ProjectFile {
                sessions: vec![session("tests", ".", "cargo nextest run")],
            },
        );

        let w = merge(&global, &wf, &ws_path, &per_project).unwrap();
        assert_eq!(w.sessions.len(), 1);
        assert_eq!(w.sessions[0].cwd, PathBuf::from("/real/project/."));
    }
}
