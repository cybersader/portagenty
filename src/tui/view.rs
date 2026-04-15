//! View-model helpers for the TUI. Takes a loaded `Workspace` plus
//! the multiplexer's current live-session list and produces a
//! renderable sequence of `SessionRow`s with state labels — what the
//! render layer and key handlers consume.
//!
//! Pure functions with no I/O; easy to unit-test without a mock
//! multiplexer or a ratatui backend.

use crate::domain::{Session, Workspace};
use crate::mux::{sanitize_session_name, SessionInfo};

/// Per-row state. Drives both the visual marker in the TUI and the
/// action Enter maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Workspace-defined session not currently running in the mpx.
    /// Enter → `create_and_attach`.
    NotStarted,
    /// Workspace-defined session that already has a live mpx session
    /// under the sanitized name. Enter → `attach`.
    Live,
    /// Live mpx session that doesn't correspond to any workspace
    /// definition. Enter → `attach`. DESIGN §9's "untracked" feature.
    Untracked,
}

impl SessionState {
    /// Short marker for the TUI. One cell wide for narrow terminals.
    pub fn marker(&self) -> &'static str {
        match self {
            SessionState::Live => "●",
            SessionState::NotStarted => "○",
            SessionState::Untracked => "?",
        }
    }

    /// Human-readable label for the row's rightmost status column.
    pub fn label(&self) -> &'static str {
        match self {
            SessionState::Live => "live",
            SessionState::NotStarted => "idle",
            SessionState::Untracked => "untracked",
        }
    }
}

/// One row in the TUI session list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// Sanitized name the mpx knows. For tracked rows this is
    /// `sanitize_session_name(session.name)`; for untracked rows it's
    /// whatever the mpx reported.
    pub mpx_name: String,
    /// Display name: the workspace's session.name (un-sanitized) for
    /// tracked rows, or the sanitized mpx name for untracked rows.
    pub display_name: String,
    pub state: SessionState,
    /// The workspace's definition, when this row maps to a tracked
    /// session. `None` for untracked rows.
    pub session: Option<Session>,
    /// Optional cwd as reported by the mpx (for untracked rows) or
    /// the workspace (for tracked rows).
    pub cwd_display: String,
    /// Optional command — from the workspace for tracked rows,
    /// `(unknown)` for untracked rows whose mpx doesn't report it.
    pub command_display: String,
}

/// Build the row list from a loaded workspace plus the mpx's current
/// sessions. Tracked rows (workspace-defined) come first in the same
/// order the workspace declared them; untracked rows follow, sorted
/// alphabetically by name for determinism.
pub fn build_rows(workspace: &Workspace, live: &[SessionInfo]) -> Vec<SessionRow> {
    let mut rows: Vec<SessionRow> = Vec::with_capacity(workspace.sessions.len() + live.len());

    // Tracked rows first.
    let live_names: std::collections::HashSet<&str> =
        live.iter().map(|s| s.name.as_str()).collect();
    for sess in &workspace.sessions {
        let mpx_name = sanitize_session_name(&sess.name);
        let state = if live_names.contains(mpx_name.as_str()) {
            SessionState::Live
        } else {
            SessionState::NotStarted
        };
        rows.push(SessionRow {
            mpx_name,
            display_name: sess.name.clone(),
            state,
            session: Some(sess.clone()),
            cwd_display: sess.cwd.display().to_string(),
            command_display: sess.command.clone(),
        });
    }

    // Untracked rows: anything in live that didn't correspond to a
    // workspace session.
    let tracked_mpx_names: std::collections::HashSet<String> = workspace
        .sessions
        .iter()
        .map(|s| sanitize_session_name(&s.name))
        .collect();
    let mut untracked: Vec<&SessionInfo> = live
        .iter()
        .filter(|s| !tracked_mpx_names.contains(&s.name))
        .collect();
    untracked.sort_by(|a, b| a.name.cmp(&b.name));

    for info in untracked {
        rows.push(SessionRow {
            mpx_name: info.name.clone(),
            display_name: info.name.clone(),
            state: SessionState::Untracked,
            session: None,
            cwd_display: info
                .cwd
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown)".into()),
            command_display: "(unknown)".into(),
        });
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Multiplexer, Session, Workspace};
    use std::path::PathBuf;

    fn ws(sessions: Vec<(&str, &str)>) -> Workspace {
        Workspace {
            name: "x".into(),
            file_path: None,
            multiplexer: Multiplexer::Tmux,
            projects: vec![],
            sessions: sessions
                .into_iter()
                .map(|(name, cmd)| Session {
                    name: name.into(),
                    cwd: PathBuf::from("/tmp"),
                    command: cmd.into(),
                })
                .collect(),
        }
    }

    fn live(names: &[&str]) -> Vec<SessionInfo> {
        names
            .iter()
            .map(|n| SessionInfo {
                name: (*n).into(),
                cwd: None,
                attached: None,
            })
            .collect()
    }

    #[test]
    fn tracked_row_is_not_started_when_mpx_has_no_match() {
        let rows = build_rows(&ws(vec![("claude", "c")]), &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, SessionState::NotStarted);
        assert_eq!(rows[0].display_name, "claude");
    }

    #[test]
    fn tracked_row_is_live_when_mpx_reports_sanitized_name() {
        let rows = build_rows(&ws(vec![("claude", "c")]), &live(&["claude"]));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, SessionState::Live);
    }

    #[test]
    fn tracked_row_is_live_when_mpx_has_sanitized_form_of_a_raw_name() {
        // Raw workspace name has spaces; mpx has the sanitized form.
        let rows = build_rows(&ws(vec![("has spaces", "c")]), &live(&["has_spaces"]));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, SessionState::Live);
        assert_eq!(rows[0].display_name, "has spaces");
        assert_eq!(rows[0].mpx_name, "has_spaces");
    }

    #[test]
    fn untracked_live_session_becomes_untracked_row() {
        let rows = build_rows(&ws(vec![]), &live(&["random-tmux-session"]));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, SessionState::Untracked);
        assert_eq!(rows[0].display_name, "random-tmux-session");
        assert!(rows[0].session.is_none());
    }

    #[test]
    fn tracked_rows_come_before_untracked() {
        let rows = build_rows(
            &ws(vec![("claude", "c"), ("tests", "t")]),
            &live(&["claude", "stranger", "another"]),
        );
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].display_name, "claude");
        assert_eq!(rows[0].state, SessionState::Live);
        assert_eq!(rows[1].display_name, "tests");
        assert_eq!(rows[1].state, SessionState::NotStarted);
        // Untracked entries sorted alphabetically.
        assert_eq!(rows[2].display_name, "another");
        assert_eq!(rows[3].display_name, "stranger");
    }

    #[test]
    fn tracked_rows_preserve_workspace_declaration_order() {
        let rows = build_rows(&ws(vec![("zzz", "z"), ("aaa", "a"), ("mmm", "m")]), &[]);
        let names: Vec<&str> = rows.iter().map(|r| r.display_name.as_str()).collect();
        assert_eq!(names, vec!["zzz", "aaa", "mmm"]);
    }

    #[test]
    fn untracked_rows_show_placeholder_for_unknown_command() {
        let rows = build_rows(&ws(vec![]), &live(&["mystery"]));
        assert_eq!(rows[0].command_display, "(unknown)");
        assert_eq!(rows[0].cwd_display, "(unknown)");
    }

    #[test]
    fn untracked_row_cwd_uses_mpx_value_when_present() {
        let info = vec![SessionInfo {
            name: "tmx".into(),
            cwd: Some(PathBuf::from("/home/u/dev")),
            attached: Some(false),
        }];
        let rows = build_rows(&ws(vec![]), &info);
        assert_eq!(rows[0].cwd_display, "/home/u/dev");
    }

    #[test]
    fn markers_differ_per_state() {
        assert_ne!(
            SessionState::Live.marker(),
            SessionState::NotStarted.marker()
        );
        assert_ne!(
            SessionState::Live.marker(),
            SessionState::Untracked.marker()
        );
        assert_ne!(
            SessionState::NotStarted.marker(),
            SessionState::Untracked.marker()
        );
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(SessionState::Live.label(), "live");
        assert_eq!(SessionState::NotStarted.label(), "idle");
        assert_eq!(SessionState::Untracked.label(), "untracked");
    }
}
