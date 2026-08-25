//! View-model helpers for the TUI. Takes a loaded `Workspace` plus
//! the multiplexer's current live-session list and produces a
//! renderable sequence of `SessionRow`s with state labels — what the
//! render layer and key handlers consume.
//!
//! Pure functions with no I/O; easy to unit-test without a mock
//! multiplexer or a ratatui backend.

use crate::domain::{Session, SessionKind, Workspace};
use crate::mux::{workspace_session_name, SessionInfo};
use crate::supervision::{LogicalSessionId, MuxTarget};

/// Per-row state. Drives both the visual marker in the TUI and the
/// action Enter maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Workspace-defined session not currently running in the mpx.
    /// Enter routes by ownership: supervised creation when eligible,
    /// otherwise ordinary `create_and_attach`.
    NotStarted,
    /// Workspace-defined session that already has a live mpx session
    /// under the sanitized name. Enter → `attach`.
    Live,
    /// Live mpx session that doesn't correspond to any workspace
    /// definition. Enter → `attach`. DESIGN §9's "untracked" feature.
    Untracked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowOwnership {
    IdleSupported,
    NeedsWorkspaceId,
    InvalidWorkspaceId,
    Owned,
    LegacyRestartRequired,
    SplitContainment,
    Pending,
    Ambiguous,
    ExistingUnverified,
    Unmanaged,
    Stale,
    Unsupported,
}

impl RowOwnership {
    pub fn label(self) -> &'static str {
        match self {
            Self::IdleSupported => "supervisable",
            Self::NeedsWorkspaceId => "needs ID",
            Self::InvalidWorkspaceId => "bad ID",
            Self::Owned => "owned",
            Self::LegacyRestartRequired => "legacy/restart",
            Self::SplitContainment => "split",
            Self::Pending => "pending",
            Self::Ambiguous => "ambiguous",
            Self::ExistingUnverified => "unverified",
            Self::Unmanaged => "unmanaged",
            Self::Stale => "stale",
            Self::Unsupported => "unsupported",
        }
    }
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
    /// Stable logical identity when the workspace has a valid UUID.
    pub logical_id: Option<LogicalSessionId>,
    /// Exact receipted target for owned private tmux/Zellij sessions.
    pub mux_target: Option<MuxTarget>,
    pub ownership: RowOwnership,
    /// Compact and expanded resource text supplied by the TUI worker.
    pub resource_summary: Option<String>,
    pub resource_details: Vec<String>,
    /// The workspace's definition, when this row maps to a tracked
    /// session. `None` for untracked rows.
    pub session: Option<Session>,
    /// Optional cwd as reported by the mpx (for untracked rows) or
    /// the workspace (for tracked rows).
    pub cwd_display: String,
    /// Optional command — from the workspace for tracked rows,
    /// `(unknown)` for untracked rows whose mpx doesn't report it.
    pub command_display: String,
    /// Optional kind hint, carried through from the workspace session
    /// when present. Drives the per-row kind marker in the TUI.
    pub kind: Option<SessionKind>,
    /// Last-attached timestamp (unix seconds) from the state store,
    /// for rendering the "2h ago" column on Live rows. `None` when the
    /// session has never been launched via `pa` (shown as blank).
    pub last_attached_unix: Option<u64>,
    /// Number of clients attached to this live mpx session, when the
    /// multiplexer exposes it (tmux does). `None` for idle/untracked
    /// rows and for mpxs that don't report per-session client counts.
    pub attached_clients: Option<u32>,
}

/// Which live mpx sessions count as "untracked" rows for this view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrackedScope {
    /// Only surface untracked sessions whose mpx name starts with
    /// this workspace's sanitized prefix (`<workspace>-…`). These are
    /// "leaked siblings" — sessions `pa` created for this workspace
    /// but that no longer match a TOML declaration (e.g. a renamed or
    /// removed session whose mpx session is still alive). Unrelated
    /// machine-wide sessions stay out of the list. The default for a
    /// real workspace.
    WorkspacePrefix,
    /// Surface every untracked live session on the machine. Used only
    /// by the picker's "live sessions on this machine" browse mode,
    /// whose whole purpose is attaching to anything running.
    All,
}

/// Build the row list from a loaded workspace plus the mpx's current
/// sessions. Tracked rows (workspace-defined) come first in the same
/// order the workspace declared them; untracked rows follow, sorted
/// alphabetically by name for determinism.
///
/// `scope` controls which live sessions become untracked rows — see
/// [`UntrackedScope`]. A real workspace passes `WorkspacePrefix` so
/// the list stays scoped to it; the live-browse pseudo-workspace
/// passes `All`.
pub fn build_rows(
    workspace: &Workspace,
    live: &[SessionInfo],
    scope: UntrackedScope,
) -> Vec<SessionRow> {
    let mut rows: Vec<SessionRow> = Vec::with_capacity(workspace.sessions.len() + live.len());

    // Tracked rows first. Each row looks up its live counterpart to
    // pick up the attached-client count; absence means NotStarted.
    for sess in &workspace.sessions {
        let mpx_name = workspace_session_name(&workspace.name, &sess.name);
        let (state, attached_clients) = live
            .iter()
            .find(|s| s.name == mpx_name)
            .map(|info| (SessionState::Live, info.attached))
            .unwrap_or((SessionState::NotStarted, None));
        let last_attached_unix = workspace
            .file_path
            .as_ref()
            .and_then(|p| crate::state::last_launch_for_session(p, &sess.name));
        let logical_id = workspace
            .id
            .as_deref()
            .and_then(|id| LogicalSessionId::new(id, sess.name.clone()).ok());
        let ownership = match (logical_id.is_some(), workspace.id.is_some(), state) {
            (false, false, _) if workspace.file_path.is_some() => RowOwnership::NeedsWorkspaceId,
            (false, true, _) => RowOwnership::InvalidWorkspaceId,
            (false, false, _) => RowOwnership::Unsupported,
            (true, _, SessionState::Live) => RowOwnership::ExistingUnverified,
            (true, _, SessionState::NotStarted) => RowOwnership::IdleSupported,
            (true, _, SessionState::Untracked) => RowOwnership::Unmanaged,
        };
        rows.push(SessionRow {
            mpx_name,
            display_name: sess.name.clone(),
            state,
            logical_id,
            mux_target: None,
            ownership,
            resource_summary: None,
            resource_details: Vec::new(),
            session: Some(sess.clone()),
            cwd_display: sess.cwd.display().to_string(),
            command_display: sess.command.clone(),
            kind: sess.kind,
            last_attached_unix,
            attached_clients,
        });
    }

    // Untracked rows: live sessions with no workspace counterpart.
    // In WorkspacePrefix scope (the default), only sessions sharing
    // this workspace's `<name>-` prefix qualify — so unrelated
    // machine-wide sessions don't clutter every workspace's list.
    let tracked_mpx_names: std::collections::HashSet<String> = workspace
        .sessions
        .iter()
        .map(|s| workspace_session_name(&workspace.name, &s.name))
        .collect();
    // `workspace_session_name(name, "")` is `sanitize(name) + "-"`;
    // trim + re-add the trailing dash so the prefix is exact.
    let prefix = format!(
        "{}-",
        workspace_session_name(&workspace.name, "").trim_end_matches('-')
    );
    let mut untracked: Vec<&SessionInfo> = live
        .iter()
        .filter(|s| !tracked_mpx_names.contains(&s.name))
        .filter(|s| match scope {
            UntrackedScope::All => true,
            UntrackedScope::WorkspacePrefix => s.name.starts_with(&prefix),
        })
        .collect();
    untracked.sort_by(|a, b| a.name.cmp(&b.name));

    for info in untracked {
        // In WorkspacePrefix scope, strip the workspace prefix for
        // display so a leaked `myproj-oldname` reads as `oldname`,
        // matching how tracked rows show the bare session name. The
        // mpx_name keeps the real name so attach/kill still target
        // the right session.
        let display_name = match scope {
            UntrackedScope::WorkspacePrefix => info
                .name
                .strip_prefix(&prefix)
                .unwrap_or(&info.name)
                .to_string(),
            UntrackedScope::All => info.name.clone(),
        };
        rows.push(SessionRow {
            mpx_name: info.name.clone(),
            display_name,
            state: SessionState::Untracked,
            logical_id: None,
            mux_target: None,
            ownership: RowOwnership::Unmanaged,
            resource_summary: None,
            resource_details: Vec::new(),
            session: None,
            cwd_display: info
                .cwd
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown)".into()),
            command_display: "(unknown)".into(),
            kind: None,
            last_attached_unix: None,
            attached_clients: info.attached,
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
            id: None,
            file_path: None,
            multiplexer: Multiplexer::Tmux,
            projects: vec![],
            sessions: sessions
                .into_iter()
                .map(|(name, cmd)| Session {
                    name: name.into(),
                    cwd: PathBuf::from("/tmp"),
                    command: cmd.into(),
                    kind: None,
                    env: std::collections::BTreeMap::new(),
                    description: None,
                })
                .collect(),
            tags: vec![],
        }
    }

    fn ws_with_kinds(sessions: Vec<(&str, Option<SessionKind>)>) -> Workspace {
        Workspace {
            name: "x".into(),
            id: None,
            file_path: None,
            multiplexer: Multiplexer::Tmux,
            projects: vec![],
            sessions: sessions
                .into_iter()
                .map(|(name, kind)| Session {
                    name: name.into(),
                    cwd: PathBuf::from("/tmp"),
                    command: "c".into(),
                    kind,
                    env: std::collections::BTreeMap::new(),
                    description: None,
                })
                .collect(),
            tags: vec![],
        }
    }

    /// Build live session infos. Names are automatically prefixed
    /// with "x-" to match the workspace-scoped mpx names that
    /// build_rows now generates (workspace "x" + session name).
    fn live(names: &[&str]) -> Vec<SessionInfo> {
        names
            .iter()
            .map(|n| SessionInfo {
                name: format!("x-{n}"),
                cwd: None,
                attached: None,
            })
            .collect()
    }

    /// Live sessions with bare names (for untracked sessions that
    /// aren't in any workspace and keep their original mpx name).
    fn live_untracked(names: &[&str]) -> Vec<SessionInfo> {
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
        let rows = build_rows(
            &ws(vec![("claude", "c")]),
            &[],
            UntrackedScope::WorkspacePrefix,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, SessionState::NotStarted);
        assert_eq!(rows[0].display_name, "claude");
    }

    #[test]
    fn tracked_row_is_live_when_mpx_reports_sanitized_name() {
        let rows = build_rows(
            &ws(vec![("claude", "c")]),
            &live(&["claude"]),
            UntrackedScope::WorkspacePrefix,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, SessionState::Live);
    }

    #[test]
    fn tracked_row_is_live_when_mpx_has_sanitized_form_of_a_raw_name() {
        // Raw workspace name has spaces; mpx has the sanitized form.
        let rows = build_rows(
            &ws(vec![("has spaces", "c")]),
            &live(&["has_spaces"]),
            UntrackedScope::WorkspacePrefix,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, SessionState::Live);
        assert_eq!(rows[0].display_name, "has spaces");
        assert_eq!(rows[0].mpx_name, "x-has_spaces");
    }

    #[test]
    fn untracked_live_session_becomes_untracked_row() {
        let rows = build_rows(
            &ws(vec![]),
            &live_untracked(&["random-tmux-session"]),
            UntrackedScope::All,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, SessionState::Untracked);
        assert_eq!(rows[0].display_name, "random-tmux-session");
        assert!(rows[0].session.is_none());
    }

    #[test]
    fn tracked_rows_come_before_untracked() {
        // Mix workspace-prefixed ("x-claude" matches tracked) with
        // bare names ("stranger", "another" = untracked).
        let mut all_live = live(&["claude"]); // "x-claude"
        all_live.extend(live_untracked(&["stranger", "another"]));
        let rows = build_rows(
            &ws(vec![("claude", "c"), ("tests", "t")]),
            &all_live,
            UntrackedScope::All,
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
        let rows = build_rows(
            &ws(vec![("zzz", "z"), ("aaa", "a"), ("mmm", "m")]),
            &[],
            UntrackedScope::WorkspacePrefix,
        );
        let names: Vec<&str> = rows.iter().map(|r| r.display_name.as_str()).collect();
        assert_eq!(names, vec!["zzz", "aaa", "mmm"]);
    }

    #[test]
    fn untracked_rows_show_placeholder_for_unknown_command() {
        let rows = build_rows(
            &ws(vec![]),
            &live_untracked(&["mystery"]),
            UntrackedScope::All,
        );
        assert_eq!(rows[0].command_display, "(unknown)");
        assert_eq!(rows[0].cwd_display, "(unknown)");
    }

    #[test]
    fn untracked_row_cwd_uses_mpx_value_when_present() {
        let info = vec![SessionInfo {
            name: "tmx".into(),
            cwd: Some(PathBuf::from("/home/u/dev")),
            attached: Some(0),
        }];
        let rows = build_rows(&ws(vec![]), &info, UntrackedScope::All);
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
        assert_eq!(RowOwnership::IdleSupported.label(), "supervisable");
        assert_eq!(RowOwnership::NeedsWorkspaceId.label(), "needs ID");
        assert_eq!(RowOwnership::InvalidWorkspaceId.label(), "bad ID");
        assert_eq!(RowOwnership::Owned.label(), "owned");
        assert_eq!(RowOwnership::ExistingUnverified.label(), "unverified");
        assert_eq!(RowOwnership::Unmanaged.label(), "unmanaged");
        assert_eq!(RowOwnership::Stale.label(), "stale");
        assert_eq!(RowOwnership::Unsupported.label(), "unsupported");
    }

    #[test]
    fn legacy_and_malformed_workspace_ids_are_not_generic_unsupported() {
        let mut legacy = ws(vec![("shell", "bash")]);
        legacy.file_path = Some(PathBuf::from("/tmp/legacy.portagenty.toml"));
        let legacy_rows = build_rows(&legacy, &[], UntrackedScope::WorkspacePrefix);
        assert_eq!(legacy_rows[0].ownership, RowOwnership::NeedsWorkspaceId);

        legacy.id = Some("not-a-uuid".into());
        let malformed_rows = build_rows(&legacy, &[], UntrackedScope::WorkspacePrefix);
        assert_eq!(
            malformed_rows[0].ownership,
            RowOwnership::InvalidWorkspaceId
        );
    }

    #[test]
    fn uuid_workspace_marks_idle_and_live_rows_honestly() {
        let mut workspace = ws(vec![("idle", "c"), ("running", "c")]);
        workspace.id = Some("550e8400-e29b-41d4-a716-446655440000".into());
        let rows = build_rows(
            &workspace,
            &live(&["running"]),
            UntrackedScope::WorkspacePrefix,
        );
        assert_eq!(rows[0].ownership, RowOwnership::IdleSupported);
        assert_eq!(rows[1].ownership, RowOwnership::ExistingUnverified);
        assert!(rows.iter().all(|row| row.logical_id.is_some()));
    }

    #[test]
    fn tracked_row_carries_kind_through_to_view() {
        let rows = build_rows(
            &ws_with_kinds(vec![
                ("claude", Some(SessionKind::ClaudeCode)),
                ("shell", None),
                ("dev", Some(SessionKind::DevServer)),
            ]),
            &[],
            UntrackedScope::WorkspacePrefix,
        );
        assert_eq!(rows[0].kind, Some(SessionKind::ClaudeCode));
        assert_eq!(rows[1].kind, None);
        assert_eq!(rows[2].kind, Some(SessionKind::DevServer));
    }

    #[test]
    fn untracked_row_always_has_no_kind() {
        let rows = build_rows(
            &ws(vec![]),
            &live_untracked(&["mystery"]),
            UntrackedScope::All,
        );
        assert_eq!(rows[0].kind, None);
    }

    #[test]
    fn workspace_prefix_scope_hides_unrelated_machine_sessions() {
        // The bug fix: a real workspace must NOT show every live mpx
        // session on the machine. Only sessions under its own prefix
        // (here "x-…") count as untracked rows.
        let mut all_live = live(&["claude"]); // "x-claude" → tracked
        all_live.extend(live_untracked(&[
            "some-other-workspace-shell", // unrelated → hidden
            "manual-tmux-thing",          // unrelated → hidden
            "x-leftover",                 // our prefix → shown
        ]));
        let rows = build_rows(
            &ws(vec![("claude", "c")]),
            &all_live,
            UntrackedScope::WorkspacePrefix,
        );
        // 1 tracked (claude) + 1 prefixed untracked (leftover).
        assert_eq!(rows.len(), 2, "unrelated sessions leaked: {rows:?}");
        assert_eq!(rows[0].display_name, "claude");
        assert_eq!(rows[0].state, SessionState::Live);
        // Untracked sibling shows with its prefix stripped, but the
        // mpx_name keeps the real session name for attach/kill.
        assert_eq!(rows[1].display_name, "leftover");
        assert_eq!(rows[1].mpx_name, "x-leftover");
        assert_eq!(rows[1].state, SessionState::Untracked);
    }

    #[test]
    fn all_scope_still_shows_every_session_for_live_browse() {
        // The picker's live-browse pseudo-workspace relies on All
        // scope to surface everything running.
        let rows = build_rows(
            &ws(vec![]),
            &live_untracked(&["alpha", "beta", "gamma"]),
            UntrackedScope::All,
        );
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.state == SessionState::Untracked));
    }
}
