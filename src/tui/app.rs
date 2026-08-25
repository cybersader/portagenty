//! TUI app state + render loop. Ratatui 0.29 + crossterm 0.28.
//!
//! v1 renders a single-column session list over the resolved
//! `domain::Workspace`. Two-pane project/session layouts and the
//! Tags / Custom Groups views come in v1.x per `ROADMAP.md`.

use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::collections::HashSet;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    prelude::*,
    widgets::{List, ListItem, ListState, Paragraph},
    DefaultTerminal,
};

use crate::domain::{Session, Workspace};
use crate::mux::{Multiplexer, SessionInfo};
use crate::supervision::{BindingReceipt, LogicalSessionId, ResourceLimits};
#[cfg(target_os = "linux")]
use crate::supervision::{MetricValue, ResourceSnapshot, SupervisionBackend};
use crate::tui::view::{build_rows, RowOwnership, SessionRow, SessionState};

/// Why the user requested a supervised launch. Only the routine Enter path is
/// eligible for an automatic ordinary fallback when non-creating preflight
/// proves supervision itself is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisionIntent {
    RoutineEnter,
    ExplicitCustom,
    StaleReplacement,
}

/// Stable identity used to restore the highlighted row after refreshes or a
/// failed launch reinitializes the workspace TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowSelectionIdentity {
    Declared {
        logical_id: Option<LogicalSessionId>,
        session_name: String,
    },
    Untracked {
        mpx_name: String,
    },
}

/// How the user wants a selected row to be realized on the mpx side.
/// Determined by the row's [`SessionState`].
#[derive(Debug, Clone)]
pub enum LaunchKind {
    /// Workspace-defined, not currently live: `create_and_attach`.
    /// `mpx_name` is the workspace-scoped name the mpx should use.
    Create { session: Session, mpx_name: String },
    /// Creation under the platform supervision backend, either from routine
    /// eligible Enter or the explicit custom-limits path.
    CreateSupervised {
        session: Session,
        limits: ResourceLimits,
        intent: SupervisionIntent,
    },
    /// Replace one exact stale receipt, create a fresh owned binding, and attach
    /// in a single confirmed flow. Stale cleanup itself never sends a signal.
    ReplaceStaleSupervised {
        session: Session,
        receipt: Box<BindingReceipt>,
        limits: ResourceLimits,
    },
    /// Already live on the workspace's ordinary shared multiplexer target.
    Attach {
        mpx_name: String,
        display_name: String,
    },
    /// Attach to the exact private target stored in a verified receipt.
    AttachOwned {
        target: crate::supervision::MuxTarget,
        display_name: String,
    },
}

/// The reason [`App::run`] returned. The outer entry point uses this
/// to decide whether to exit silently or hand off to the multiplexer.
#[derive(Debug, Clone)]
pub enum AppOutcome {
    Quit,
    /// User pressed Esc to go back. The outer driver routes this:
    /// - if the picker was the entry point, re-open it;
    /// - otherwise (walk-up path) treat the same as Quit.
    Back,
    Launch(LaunchKind),
    /// User pressed `o` — exit pa entirely and spawn a plain shell
    /// at the given directory. No mpx, no session, no state. Like
    /// "Open in Terminal" from a file manager.
    OpenShellAt(std::path::PathBuf),
}

/// Final state handed back to the outer coordinator after the event loop.
/// The workspace may differ from the input when the TUI upgrades a legacy
/// manifest with a stable UUID before launching a supervised session.
pub struct AppRunResult {
    pub outcome: AppOutcome,
    pub mux: Box<dyn Multiplexer>,
    pub workspace: Workspace,
}

/// Internal action dispatch. Returned from [`App::handle_key`] so the
/// event loop can translate a key press into either continued
/// in-TUI work or a reason to exit the loop.
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Quit,
    /// Esc pressed — ask the outer driver to back out to the picker
    /// (or quit if the picker wasn't in the chain).
    Back,
    LaunchSelected,
    LaunchSupervisedSelected(ResourceLimits),
    LaunchStaleSupervised {
        session: Session,
        receipt: Box<BindingReceipt>,
        limits: ResourceLimits,
    },
    /// `o` pressed — ask the outer driver to exit pa and spawn a
    /// plain shell at the given directory. From the session list's
    /// bare `o` this is the workspace's dir; from the file-tree
    /// browser's `o` it's the highlighted folder.
    OpenShellAt(std::path::PathBuf),
}

/// Top-level TUI state. Holds everything the event loop needs; no
/// globals, nothing static. Tests construct `App` directly and render
/// into a `ratatui::backend::TestBackend`.
pub struct App {
    workspace: Workspace,
    mux: Box<dyn Multiplexer>,
    rows: Vec<SessionRow>,
    list_state: ListState,
    should_quit: bool,
    /// True while the `?` help overlay is visible. While open, key
    /// handling is short-circuited: any key press closes the overlay
    /// and returns `Action::None` (no accidental nav / launch).
    help_open: bool,
    /// When Some, a confirm modal is showing for the given action.
    /// Key handling diverts to the confirm classifier; on y/Y we
    /// perform the action, on anything else we clear and continue.
    pending: Option<PendingAction>,
    /// Human-readable status blurb shown in the footer region. Set
    /// after row actions (e.g. "deleted 'claude'"). Auto-clears
    /// after STATUS_TTL via the event-poll loop, so it doesn't
    /// linger when the user just walks away.
    status: Option<String>,
    /// Wall-clock instant the current status was set. `None` when
    /// status is `None`. Used by the run loop to age status messages
    /// out without requiring a keystroke.
    status_set_at: Option<std::time::Instant>,
    /// In-TUI session edit overlay. While `Some`, key handling is
    /// diverted to `crate::tui::edit::handle_key` and the row list
    /// renders normally underneath. Mutually exclusive with `pending`.
    editing: Option<crate::tui::edit::EditState>,
    /// When Some, the find overlay is open for cwd selection.
    /// Tuple: (session_name being edited, search state).
    browsing_cwd: Option<(String, crate::tui::find::SearchState)>,
    /// When Some, the find overlay is open for general file-tree
    /// browsing (not tied to a session edit). Opened via `t` on the
    /// session list. OpenShellAt is the primary action from this
    /// overlay — drop to shell at the highlighted folder.
    browsing: Option<crate::tui::find::SearchState>,
    /// When Some, the "add new session" modal is showing. Two-stage:
    /// first name, then command. Enter advances or commits; Esc
    /// cancels.
    adding_session: Option<AddSessionState>,
    /// Which live mpx sessions show up as untracked rows. Real
    /// workspaces scope to their own `<name>-` prefix so unrelated
    /// machine sessions don't clutter the list; the live-browse
    /// pseudo-workspace shows everything. Decided once at
    /// construction from whether the workspace has a file on disk.
    untracked_scope: crate::tui::view::UntrackedScope,
    /// When true, the highlighted row expands in place to show its
    /// full description, real command, and cwd on dim labeled lines.
    /// Default on; `z` toggles it off for max-density scanning.
    /// Session-local (not persisted) for now.
    expand_selected: bool,
    receipts: BTreeMap<LogicalSessionId, BindingReceipt>,
    #[cfg(target_os = "linux")]
    pending_launches: BTreeMap<LogicalSessionId, crate::supervision::PendingLaunch>,
    #[cfg(target_os = "linux")]
    resource_snapshots: BTreeMap<LogicalSessionId, ResourceSnapshot>,
    #[cfg(target_os = "linux")]
    resource_refresh_pending: HashSet<LogicalSessionId>,
    #[cfg(target_os = "linux")]
    last_resource_sample: std::time::Instant,
    #[cfg(target_os = "linux")]
    resource_worker: Option<crate::tui::resources::ResourceWorker>,
    supervising: Option<SuperviseState>,
    supervision_preflight: fn() -> Result<()>,
    supervision_evidence_available: bool,
}

/// Two-stage state for the "add new session" modal.
#[derive(Debug, Clone)]
struct AddSessionState {
    stage: AddStage,
    name: String,
    command: String,
    /// Transient error from the last failed commit (e.g. duplicate).
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddStage {
    Name,
    Command,
}

fn supervise_limit_fields(limits: &ResourceLimits) -> (String, String, String, String, String) {
    const GIB: u64 = 1024 * 1024 * 1024;
    let memory_high = limits
        .memory_high_bytes
        .map(|bytes| {
            if bytes % GIB == 0 {
                format!("{}G", bytes / GIB)
            } else {
                bytes.to_string()
            }
        })
        .unwrap_or_default();
    let memory_max = limits
        .memory_max_bytes
        .map(|bytes| {
            if bytes % GIB == 0 {
                format!("{}G", bytes / GIB)
            } else {
                bytes.to_string()
            }
        })
        .unwrap_or_default();
    let memory_swap_max = limits
        .memory_swap_max_bytes
        .map(|bytes| {
            const MIB: u64 = 1024 * 1024;
            if bytes % GIB == 0 {
                format!("{}G", bytes / GIB)
            } else if bytes % MIB == 0 {
                format!("{}MiB", bytes / MIB)
            } else {
                bytes.to_string()
            }
        })
        .unwrap_or_default();
    let cpu_quota = limits
        .cpu_quota_percent
        .map(|value| value.to_string())
        .unwrap_or_default();
    let tasks_max = limits
        .tasks_max
        .map(|value| value.to_string())
        .unwrap_or_default();
    (
        memory_high,
        memory_max,
        memory_swap_max,
        cpu_quota,
        tasks_max,
    )
}

#[cfg(not(test))]
fn default_supervision_preflight() -> Result<()> {
    let report = crate::supervision::platform_backend().capabilities();
    if report.overall != crate::supervision::CapabilityState::Supported {
        anyhow::bail!("resource supervision is unavailable: {:?}", report.overall);
    }
    Ok(())
}

#[cfg(test)]
fn default_supervision_preflight() -> Result<()> {
    Ok(())
}

#[derive(Debug, Clone)]
struct SuperviseState {
    stage: SuperviseStage,
    session_name: String,
    mpx_name: String,
    memory_high: String,
    memory_max: String,
    memory_swap_max: String,
    cpu_quota: String,
    tasks_max: String,
    stale_receipt: Option<Box<BindingReceipt>>,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuperviseStage {
    MemoryHigh,
    MemoryMax,
    MemorySwapMax,
    CpuQuota,
    TasksMax,
}

/// How long status messages stick around before auto-clearing.
const STATUS_TTL: std::time::Duration = std::time::Duration::from_millis(2500);

/// Queued destructive action awaiting user confirmation.
#[derive(Debug, Clone)]
enum PendingAction {
    /// Remove the named session from the workspace file on disk.
    DeleteSession { name: String },
    /// Terminate the live mpx session (tmux kill-session / zellij
    /// kill-session + delete). Does not touch the workspace file.
    KillSession {
        /// Display name for the status line (workspace name for
        /// tracked rows, mpx name for untracked).
        display_name: String,
        /// Sanitized name the multiplexer knows.
        mpx_name: String,
        /// Client count if the mpx reported it. Used to warn users
        /// about disconnecting other devices.
        attached_clients: Option<u32>,
    },
    /// Prepare a declared row for a fresh supervised launch. A legacy
    /// workspace may first receive a stable UUID; a live ordinary target is
    /// then terminated by its exact mpx name and revalidated as idle. The old
    /// process tree is never claimed or migrated.
    PrepareSupervised {
        session_name: String,
        display_name: String,
        mpx_name: String,
        attached_clients: Option<u32>,
        assign_workspace_id: bool,
        restart_live: bool,
    },
    #[cfg(target_os = "linux")]
    StopOwned {
        display_name: String,
        receipt: Box<BindingReceipt>,
        force: bool,
    },
    /// Remove one exact receipt only after the backend proves both its
    /// systemd invocation and private multiplexer target are absent.
    /// This action never sends a signal.
    #[cfg(target_os = "linux")]
    RemoveStaleReceipt {
        display_name: String,
        receipt: Box<BindingReceipt>,
    },
    /// Confirm one exact stale receipt replacement. Execution is handed to the
    /// outer launch coordinator so cleanup, creation, and attach remain one flow.
    ReplaceStaleBinding {
        session: Session,
        receipt: Box<BindingReceipt>,
        limits: ResourceLimits,
    },
    /// Switch the workspace's pinned multiplexer between tmux and
    /// zellij. Edits the TOML in place via toml_edit (preserves
    /// comments + sessions). Doesn't touch any live mpx sessions
    /// already running — those stay in the old mpx and reappear
    /// as Untracked rows.
    SwitchMpx {
        /// Multiplexer the workspace is currently pinned to.
        from: crate::domain::Multiplexer,
        /// Multiplexer to switch to.
        to: crate::domain::Multiplexer,
        /// How many sessions in the current mpx are live; included
        /// in the confirm prompt as a "you'll orphan N sessions"
        /// warning.
        live_in_current: usize,
    },
}

impl App {
    /// Construct with the workspace + mpx, plus the pre-fetched live
    /// session list. Passing `live` in explicitly keeps `new` pure
    /// (no I/O at construction time) and lets tests drive any
    /// rendering state they want without mockall expectations.
    pub fn new(workspace: Workspace, mux: Box<dyn Multiplexer>, live: Vec<SessionInfo>) -> Self {
        // A real workspace (loaded from a file) scopes untracked rows
        // to its own prefix; the live-browse pseudo-workspace (no
        // file on disk) shows every live session so you can attach to
        // anything running.
        let untracked_scope = if workspace.file_path.is_some() {
            crate::tui::view::UntrackedScope::WorkspacePrefix
        } else {
            crate::tui::view::UntrackedScope::All
        };
        let rows = build_rows(&workspace, &live, untracked_scope);
        let mut list_state = ListState::default();
        if !rows.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            workspace,
            mux,
            rows,
            list_state,
            should_quit: false,
            help_open: false,
            pending: None,
            status: None,
            status_set_at: None,
            editing: None,
            browsing_cwd: None,
            browsing: None,
            adding_session: None,
            untracked_scope,
            expand_selected: true,
            receipts: BTreeMap::new(),
            #[cfg(target_os = "linux")]
            pending_launches: BTreeMap::new(),
            #[cfg(target_os = "linux")]
            resource_snapshots: BTreeMap::new(),
            #[cfg(target_os = "linux")]
            resource_refresh_pending: HashSet::new(),
            #[cfg(target_os = "linux")]
            last_resource_sample: std::time::Instant::now(),
            #[cfg(target_os = "linux")]
            resource_worker: None,
            supervising: None,
            supervision_preflight: default_supervision_preflight,
            supervision_evidence_available: true,
        }
    }

    #[cfg(target_os = "linux")]
    pub fn with_receipts(self, receipts: Vec<BindingReceipt>) -> Self {
        self.with_supervision_evidence(receipts, Vec::new())
    }

    #[cfg(target_os = "linux")]
    pub fn with_supervision_evidence(
        mut self,
        receipts: Vec<BindingReceipt>,
        pending_launches: Vec<crate::supervision::PendingLaunch>,
    ) -> Self {
        self.receipts = receipts
            .into_iter()
            .map(|receipt| (receipt.logical_id.clone(), receipt))
            .collect();
        self.pending_launches = pending_launches
            .into_iter()
            .map(|pending| (pending.logical_id.clone(), pending))
            .collect();
        self.apply_receipt_annotations(&BTreeMap::new());
        self.apply_pending_annotations();
        if !self.receipts.is_empty() {
            self.resource_worker = Some(crate::tui::resources::ResourceWorker::start());
            self.request_all_resource_refreshes();
        }
        self
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn workspace_id(&self) -> Option<&str> {
        self.workspace.id.as_deref()
    }

    fn has_supervision_evidence(&self, logical_id: &LogicalSessionId) -> bool {
        if self.receipts.contains_key(logical_id) {
            return true;
        }
        #[cfg(target_os = "linux")]
        {
            self.pending_launches.contains_key(logical_id)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    #[cfg(target_os = "linux")]
    fn current_receipt_annotations(
        &self,
    ) -> BTreeMap<LogicalSessionId, (RowOwnership, Option<String>, Vec<String>)> {
        self.rows
            .iter()
            .filter_map(|row| {
                let logical_id = row.logical_id.as_ref()?;
                self.receipts.contains_key(logical_id).then(|| {
                    (
                        logical_id.clone(),
                        (
                            row.ownership,
                            row.resource_summary.clone(),
                            row.resource_details.clone(),
                        ),
                    )
                })
            })
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn apply_receipt_annotations(
        &mut self,
        annotations: &BTreeMap<LogicalSessionId, (RowOwnership, Option<String>, Vec<String>)>,
    ) {
        for receipt in self.receipts.values() {
            let target_name = match &receipt.mux_target {
                crate::supervision::MuxTarget::TmuxPrivate { session, .. }
                | crate::supervision::MuxTarget::TmuxShared { session }
                | crate::supervision::MuxTarget::Zellij { session, .. } => session.clone(),
            };
            let (ownership, resource_summary, resource_details) = annotations
                .get(&receipt.logical_id)
                .cloned()
                .unwrap_or((RowOwnership::ExistingUnverified, None, Vec::new()));
            if let Some(row) = self
                .rows
                .iter_mut()
                .find(|row| row.logical_id.as_ref() == Some(&receipt.logical_id))
            {
                // A receipt is only authoritative after exact runtime
                // reconciliation succeeds. Until then, and after it becomes
                // stale, preserve the state/name derived from a fresh ordinary
                // multiplexer listing so Enter can never chase a dead opaque
                // target.
                if matches!(
                    ownership,
                    RowOwnership::Owned
                        | RowOwnership::LegacyRestartRequired
                        | RowOwnership::SplitContainment
                ) {
                    row.state = SessionState::Live;
                    row.mpx_name = target_name;
                    row.mux_target = Some(receipt.mux_target.clone());
                }
                row.ownership = ownership;
                row.resource_summary = resource_summary;
                row.resource_details = resource_details;
            } else {
                self.rows.push(SessionRow {
                    mpx_name: target_name,
                    display_name: receipt.logical_id.session_name.clone(),
                    state: SessionState::Untracked,
                    logical_id: Some(receipt.logical_id.clone()),
                    mux_target: Some(receipt.mux_target.clone()),
                    ownership,
                    resource_summary,
                    resource_details,
                    session: None,
                    cwd_display: "(receipt orphan)".into(),
                    command_display: "(receipt only)".into(),
                    kind: None,
                    last_attached_unix: None,
                    attached_clients: None,
                });
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn apply_pending_annotations(&mut self) {
        for pending in self.pending_launches.values() {
            let target_name = match &pending.mux_target {
                crate::supervision::MuxTarget::TmuxPrivate { session, .. }
                | crate::supervision::MuxTarget::TmuxShared { session }
                | crate::supervision::MuxTarget::Zellij { session, .. } => session.clone(),
            };
            let mut details = vec![
                format!("pending unit: {}", pending.unit_name),
                format!("pending target: {:?}", pending.mux_target),
                format!("pending marker: {}", pending.marker_path.display()),
                format!(
                    "creator: pid={} start-time-ticks={}",
                    pending.creator_pid, pending.creator_start_time_ticks
                ),
                "attach, creation, fallback, stop, and force-kill are blocked; use `pa resources status` and signal-free `pa resources cleanup` only when proven dead".into(),
            ];
            if let Some(error) = &pending.last_error {
                details.push(format!("last error: {error}"));
            }
            if let Some(row) = self
                .rows
                .iter_mut()
                .find(|row| row.logical_id.as_ref() == Some(&pending.logical_id))
            {
                row.ownership = RowOwnership::Pending;
                row.resource_summary = Some("pending launch (fail-closed)".into());
                row.resource_details = details;
            } else {
                self.rows.push(SessionRow {
                    mpx_name: target_name,
                    display_name: pending.logical_id.session_name.clone(),
                    state: SessionState::Untracked,
                    logical_id: Some(pending.logical_id.clone()),
                    mux_target: None,
                    ownership: RowOwnership::Pending,
                    resource_summary: Some("pending launch (fail-closed)".into()),
                    resource_details: details,
                    session: None,
                    cwd_display: "(pending launch)".into(),
                    command_display: "(pending evidence only)".into(),
                    kind: None,
                    last_attached_unix: None,
                    attached_clients: None,
                });
            }
        }
    }

    fn rebuild_rows(&mut self, live: &[SessionInfo]) {
        let selection = self.selection_identity();
        #[cfg(target_os = "linux")]
        let annotations = self.current_receipt_annotations();
        self.rows = build_rows(&self.workspace, live, self.untracked_scope);
        #[cfg(target_os = "linux")]
        {
            self.apply_receipt_annotations(&annotations);
            self.apply_pending_annotations();
        }
        if let Some(selection) = selection {
            self.restore_selection(&selection);
        } else if self.rows.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(0));
        }
    }

    pub fn selection_identity(&self) -> Option<RowSelectionIdentity> {
        let row = self.selected_row()?;
        match &row.session {
            Some(session) => Some(RowSelectionIdentity::Declared {
                logical_id: row.logical_id.clone(),
                session_name: session.name.clone(),
            }),
            None => Some(RowSelectionIdentity::Untracked {
                mpx_name: row.mpx_name.clone(),
            }),
        }
    }

    pub fn restore_selection(&mut self, selection: &RowSelectionIdentity) {
        let index = match selection {
            RowSelectionIdentity::Declared {
                logical_id,
                session_name,
            } => logical_id
                .as_ref()
                .and_then(|logical_id| {
                    self.rows
                        .iter()
                        .position(|row| row.logical_id.as_ref() == Some(logical_id))
                })
                .or_else(|| {
                    self.rows.iter().position(|row| {
                        row.session
                            .as_ref()
                            .is_some_and(|session| session.name == *session_name)
                    })
                }),
            RowSelectionIdentity::Untracked { mpx_name } => self
                .rows
                .iter()
                .position(|row| row.session.is_none() && row.mpx_name == *mpx_name),
        };
        if self.rows.is_empty() {
            self.list_state.select(None);
        } else {
            let fallback = self
                .list_state
                .selected()
                .unwrap_or(0)
                .min(self.rows.len() - 1);
            self.list_state.select(Some(index.unwrap_or(fallback)));
        }
    }

    pub fn set_persistent_status(&mut self, msg: impl Into<String>) {
        self.status = Some(msg.into());
        self.status_set_at = None;
    }

    pub fn fail_closed_supervision(&mut self, msg: impl Into<String>) {
        self.supervision_evidence_available = false;
        self.set_persistent_status(msg);
    }

    /// Currently-selected row index, if any.
    pub fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }

    /// Read-only view of the rows. Useful for tests + future TUI
    /// features that need to reason about the full view-model.
    pub fn rows(&self) -> &[SessionRow] {
        &self.rows
    }

    fn select_next(&mut self) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let sel = self.list_state.selected().unwrap_or(0);
        self.list_state.select(Some((sel + 1) % n));
    }

    fn select_prev(&mut self) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let sel = self.list_state.selected().unwrap_or(0);
        let next = if sel == 0 { n - 1 } else { sel - 1 };
        self.list_state.select(Some(next));
    }

    fn select_first(&mut self) {
        if !self.rows.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        let n = self.rows.len();
        if n > 0 {
            self.list_state.select(Some(n - 1));
        }
    }

    /// Consume the app: run the event loop until the user either quits
    /// or picks a session to launch. Returns the outcome, multiplexer, and
    /// final workspace so the outer entry point sees in-TUI manifest upgrades
    /// before it launches or resumes after a setup failure.
    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<AppRunResult> {
        loop {
            // Auto-age the status line so a "cancelled" or "deleted X"
            // message doesn't sit forever when the user walks away.
            if let Some(set_at) = self.status_set_at {
                if set_at.elapsed() >= STATUS_TTL {
                    self.clear_status();
                }
            }
            #[cfg(target_os = "linux")]
            {
                self.poll_resource_results();
                if self.last_resource_sample.elapsed() >= std::time::Duration::from_secs(2) {
                    self.request_all_resource_refreshes();
                    self.last_resource_sample = std::time::Instant::now();
                }
            }
            terminal.draw(|frame| self.render(frame))?;

            // Poll instead of read so we can re-check the status TTL
            // periodically. 250ms is short enough to feel responsive
            // when the message clears, long enough that we're not
            // burning CPU.
            if event::poll(std::time::Duration::from_millis(250))? {
                if let Some(outcome) = self.handle_event()? {
                    return Ok(self.finish(outcome));
                }
            }
        }
    }

    fn finish(self, outcome: AppOutcome) -> AppRunResult {
        AppRunResult {
            outcome,
            mux: self.mux,
            workspace: self.workspace,
        }
    }

    fn handle_event(&mut self) -> Result<Option<AppOutcome>> {
        let Event::Key(key) = event::read()? else {
            return Ok(None);
        };
        if key.kind != KeyEventKind::Press {
            return Ok(None);
        }
        let action = self.handle_key(key.code, key.modifiers);
        Ok(self.reduce_action(action))
    }

    fn reduce_action(&mut self, action: Action) -> Option<AppOutcome> {
        match action {
            Action::None => None,
            Action::Quit => Some(AppOutcome::Quit),
            Action::Back => Some(AppOutcome::Back),
            Action::LaunchSelected => {
                let i = self.selected()?;
                let row = self.rows.get(i)?;
                let receipt_backed = row
                    .logical_id
                    .as_ref()
                    .is_some_and(|logical_id| self.has_supervision_evidence(logical_id));
                let ownership = row.ownership;
                let state = row.state;
                let session = row.session.clone();
                let logical_id = row.logical_id.clone();
                let mpx_name = row.mpx_name.clone();
                let display_name = row.display_name.clone();
                let mux_target = row.mux_target.clone();

                if ownership == RowOwnership::Pending {
                    self.set_status(
                        "Enter: pending supervision evidence blocks attach, fallback, and creation; inspect with `pa resources status`",
                    );
                    return None;
                }
                if receipt_backed
                    && !matches!(
                        ownership,
                        RowOwnership::Owned
                            | RowOwnership::LegacyRestartRequired
                            | RowOwnership::SplitContainment
                            | RowOwnership::Stale
                    )
                {
                    self.set_status(
                        "Enter: verifying the ownership receipt; wait for the row to become owned or stale",
                    );
                    return None;
                }
                if matches!(
                    ownership,
                    RowOwnership::Owned
                        | RowOwnership::LegacyRestartRequired
                        | RowOwnership::SplitContainment
                ) {
                    return Some(AppOutcome::Launch(LaunchKind::AttachOwned {
                        target: mux_target?,
                        display_name,
                    }));
                }
                if matches!(state, SessionState::Live | SessionState::Untracked) {
                    return Some(AppOutcome::Launch(LaunchKind::Attach {
                        mpx_name,
                        display_name,
                    }));
                }
                let session = session?;
                if ownership == RowOwnership::IdleSupported && !self.supervision_evidence_available
                {
                    self.set_status(
                        "Enter: supervision evidence could not be loaded, so supervised creation and ordinary fallback are blocked",
                    );
                    return None;
                }
                if ownership == RowOwnership::Stale {
                    let Some(logical_id) = logical_id else {
                        self.set_status(
                            "Enter: stale row has no declared logical identity; use x for cleanup",
                        );
                        return None;
                    };
                    let Some(receipt) = self.receipts.get(&logical_id).cloned() else {
                        self.set_status(
                            "Enter: stale row is missing its exact receipt; refresh and retry",
                        );
                        return None;
                    };
                    let limits = if receipt.limits.is_empty() {
                        ResourceLimits::defaults_for_kind(session.kind)
                    } else {
                        receipt.limits.clone()
                    };
                    self.pending = Some(PendingAction::ReplaceStaleBinding {
                        session,
                        receipt: Box::new(receipt),
                        limits,
                    });
                    return None;
                }
                let session_kind = session.kind;
                let kind = if ownership == RowOwnership::IdleSupported {
                    LaunchKind::CreateSupervised {
                        session,
                        limits: ResourceLimits::defaults_for_kind(session_kind),
                        intent: SupervisionIntent::RoutineEnter,
                    }
                } else {
                    LaunchKind::Create { session, mpx_name }
                };
                Some(AppOutcome::Launch(kind))
            }
            Action::LaunchSupervisedSelected(limits) => self.selected().and_then(|i| {
                let row = self.rows.get(i)?;
                row.session
                    .as_ref()
                    .map(|session| LaunchKind::CreateSupervised {
                        session: session.clone(),
                        limits,
                        intent: SupervisionIntent::ExplicitCustom,
                    })
                    .map(AppOutcome::Launch)
            }),
            Action::LaunchStaleSupervised {
                session,
                receipt,
                limits,
            } => Some(AppOutcome::Launch(LaunchKind::ReplaceStaleSupervised {
                session,
                receipt,
                limits,
            })),
            Action::OpenShellAt(dir) => Some(AppOutcome::OpenShellAt(dir)),
        }
    }

    /// The workspace's "natural cwd" — the directory containing its
    /// *.portagenty.toml file, with fallbacks to the first session's
    /// cwd, then HOME, then ".". Used by `o` and `t` to choose a
    /// sensible starting point.
    fn workspace_dir(&self) -> std::path::PathBuf {
        self.workspace
            .file_path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .or_else(|| self.workspace.sessions.first().map(|s| s.cwd.clone()))
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
            })
    }

    /// Open the file-tree browser rooted at the workspace's dir.
    /// Triggered by `t` in the session list.
    fn open_file_tree(&mut self) {
        self.browsing = Some(crate::tui::find::SearchState::tree_at(self.workspace_dir()));
    }

    /// The currently-selected row, if any. Exposed so the outer entry
    /// point can ask "what did the user pick?" after `run` returns.
    pub fn selected_row(&self) -> Option<&SessionRow> {
        self.selected().and_then(|i| self.rows.get(i))
    }

    /// Set the footer status line + reset its TTL clock. Use this
    /// instead of writing to `self.status` directly so auto-clear
    /// timing stays consistent.
    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some(msg.into());
        self.status_set_at = Some(std::time::Instant::now());
    }

    fn clear_status(&mut self) {
        self.status = None;
        self.status_set_at = None;
    }

    #[cfg(target_os = "linux")]
    fn request_resource_refresh(&mut self, logical_id: &LogicalSessionId) -> bool {
        if self.pending_launches.contains_key(logical_id)
            || self.resource_refresh_pending.contains(logical_id)
        {
            return false;
        }
        let Some(receipt) = self.receipts.get(logical_id).cloned() else {
            return false;
        };
        let previous = self.resource_snapshots.get(logical_id).cloned();
        let queued = self
            .resource_worker
            .as_ref()
            .is_some_and(|worker| worker.request(receipt, previous));
        if queued {
            self.resource_refresh_pending.insert(logical_id.clone());
        }
        queued
    }

    #[cfg(target_os = "linux")]
    fn request_all_resource_refreshes(&mut self) {
        let ids: Vec<LogicalSessionId> = self.receipts.keys().cloned().collect();
        for logical_id in ids {
            let _ = self.request_resource_refresh(&logical_id);
        }
    }

    #[cfg(target_os = "linux")]
    fn request_selected_resource_refresh(&mut self) {
        let logical_id = self.selected_row().and_then(|row| row.logical_id.clone());
        if let Some(logical_id) = logical_id {
            if self.request_resource_refresh(&logical_id) {
                self.set_status("resource refresh queued");
            } else {
                self.set_status("r: refresh not queued (missing, busy, or already pending)");
            }
        } else {
            self.set_status("r: selected row has no owned resource binding");
        }
    }

    #[cfg(target_os = "linux")]
    fn poll_resource_results(&mut self) {
        let results: Vec<_> = self
            .resource_worker
            .as_ref()
            .map(|worker| worker.drain().collect())
            .unwrap_or_default();
        let mut rebuild_from_mux = false;
        for result in results {
            self.resource_refresh_pending.remove(&result.logical_id);
            if self.pending_launches.contains_key(&result.logical_id) {
                continue;
            }
            let ownership = match &result.ownership {
                crate::supervision::OwnershipState::OwnedVerified(_) => RowOwnership::Owned,
                crate::supervision::OwnershipState::LegacyRestartRequired(_) => {
                    RowOwnership::LegacyRestartRequired
                }
                crate::supervision::OwnershipState::SplitContainment(_) => {
                    RowOwnership::SplitContainment
                }
                crate::supervision::OwnershipState::AmbiguousBinding(_) => {
                    rebuild_from_mux = true;
                    RowOwnership::Ambiguous
                }
                crate::supervision::OwnershipState::StaleBinding(_) => {
                    rebuild_from_mux = true;
                    RowOwnership::Stale
                }
                crate::supervision::OwnershipState::ExistingUnverified => {
                    RowOwnership::ExistingUnverified
                }
                crate::supervision::OwnershipState::Unmanaged => RowOwnership::Unmanaged,
                crate::supervision::OwnershipState::Unsupported(_) => RowOwnership::Unsupported,
                crate::supervision::OwnershipState::IdleSupported => RowOwnership::IdleSupported,
            };
            let previous = self.resource_snapshots.get(&result.logical_id);
            let event_notice = result
                .snapshot
                .as_ref()
                .and_then(|snapshot| resource_event_notice(previous, snapshot));
            if let Some(snapshot) = result.snapshot {
                self.resource_snapshots
                    .insert(result.logical_id.clone(), snapshot.clone());
                if let Some(row) = self
                    .rows
                    .iter_mut()
                    .find(|row| row.logical_id.as_ref() == Some(&result.logical_id))
                {
                    row.resource_summary = Some(resource_summary(&snapshot));
                    row.resource_details = resource_details(&snapshot);
                }
            }
            if let Some(row) = self
                .rows
                .iter_mut()
                .find(|row| row.logical_id.as_ref() == Some(&result.logical_id))
            {
                row.ownership = ownership;
                if matches!(
                    ownership,
                    RowOwnership::Owned
                        | RowOwnership::LegacyRestartRequired
                        | RowOwnership::SplitContainment
                ) {
                    if let Some(receipt) = self.receipts.get(&result.logical_id) {
                        row.state = SessionState::Live;
                        row.mpx_name = match &receipt.mux_target {
                            crate::supervision::MuxTarget::TmuxPrivate { session, .. }
                            | crate::supervision::MuxTarget::TmuxShared { session }
                            | crate::supervision::MuxTarget::Zellij { session, .. } => {
                                session.clone()
                            }
                        };
                        row.mux_target = Some(receipt.mux_target.clone());
                    }
                }
                if let Some(error) = result.error {
                    row.resource_summary = Some(format!("resource error: {error}"));
                }
            }
            if let Some(notice) = event_notice {
                self.set_status(format!("resource event: {notice}"));
            }
        }
        if rebuild_from_mux {
            if let Ok(live) = self.mux.list_sessions() {
                self.rebuild_rows(&live);
            }
        }
    }

    /// Queue a delete-session confirm modal for the currently-selected
    /// row. Only valid on tracked rows (ones with a workspace
    /// session). Untracked rows (live mpx sessions outside the
    /// workspace) are ignored — delete means "remove from workspace
    /// TOML", and they're not in the TOML to begin with.
    /// Queue a kill-session confirm modal. Valid on Live or Untracked
    /// rows (both have a live mpx session to terminate). NotStarted
    /// rows have no mpx session, so kill is a no-op — we short-circuit
    /// with a status message.
    fn open_kill_prompt(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let receipt_backed = row
            .logical_id
            .as_ref()
            .is_some_and(|logical_id| self.has_supervision_evidence(logical_id));
        if receipt_backed
            && !matches!(
                row.ownership,
                RowOwnership::Owned
                    | RowOwnership::LegacyRestartRequired
                    | RowOwnership::SplitContainment
                    | RowOwnership::Pending
                    | RowOwnership::Ambiguous
                    | RowOwnership::Stale
            )
        {
            self.set_status("x: verifying the ownership receipt; wait for a final state");
            return;
        }
        match row.ownership {
            #[cfg(target_os = "linux")]
            RowOwnership::Owned => {
                let Some(logical_id) = row.logical_id.clone() else {
                    self.set_status("x: owned row is missing its logical identity");
                    return;
                };
                let Some(receipt) = self.receipts.get(&logical_id).cloned() else {
                    self.set_status("x: owned row is missing its binding receipt");
                    return;
                };
                self.pending = Some(PendingAction::StopOwned {
                    display_name: row.display_name.clone(),
                    receipt: Box::new(receipt),
                    force: false,
                });
            }
            #[cfg(not(target_os = "linux"))]
            RowOwnership::Owned => {
                self.set_status(
                    "x: receipt-backed resource control is unsupported on this platform",
                );
            }
            RowOwnership::LegacyRestartRequired => {
                self.set_status(
                    "x: legacy v1 service is attach-only; exit it normally to transition",
                );
            }
            RowOwnership::SplitContainment => {
                self.set_status(
                    "x: split containment disables whole-workload stop; external descendants remain separately bounded",
                );
            }
            RowOwnership::Pending => {
                self.set_status(
                    "x: pending launch evidence is not an owned control target; use `pa resources status` and signal-free cleanup only when proven dead",
                );
            }
            RowOwnership::Ambiguous => {
                self.set_status("x: ownership is ambiguous; no control action is allowed");
            }
            #[cfg(target_os = "linux")]
            RowOwnership::Stale => {
                let Some(logical_id) = row.logical_id.clone() else {
                    self.set_status("x: stale row is missing its logical identity");
                    return;
                };
                let Some(receipt) = self.receipts.get(&logical_id).cloned() else {
                    self.set_status("x: stale row is missing its binding receipt");
                    return;
                };
                self.pending = Some(PendingAction::RemoveStaleReceipt {
                    display_name: row.display_name.clone(),
                    receipt: Box::new(receipt),
                });
            }
            #[cfg(not(target_os = "linux"))]
            RowOwnership::Stale => {
                self.set_status("x: stale receipt cleanup is unsupported on this platform");
            }
            _ if row.state == SessionState::NotStarted => {
                self.set_status("x: no live session to stop on this row (it's idle)");
            }
            _ => {
                self.pending = Some(PendingAction::KillSession {
                    display_name: row.display_name.clone(),
                    mpx_name: row.mpx_name.clone(),
                    attached_clients: row.attached_clients,
                });
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn open_force_kill_prompt(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.ownership != RowOwnership::Owned {
            self.set_status("X: force-kill is available only for owned-and-verified rows");
            return;
        }
        let Some(logical_id) = row.logical_id.clone() else {
            self.set_status("X: owned row is missing its logical identity");
            return;
        };
        let Some(receipt) = self.receipts.get(&logical_id).cloned() else {
            self.set_status("X: owned row is missing its binding receipt");
            return;
        };
        self.pending = Some(PendingAction::StopOwned {
            display_name: row.display_name.clone(),
            receipt: Box::new(receipt),
            force: true,
        });
    }

    #[cfg(not(target_os = "linux"))]
    fn open_force_kill_prompt(&mut self) {
        self.set_status("X: receipt-backed force-kill is unsupported on this platform");
    }

    fn open_supervise_modal(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let Some(session) = row.session.as_ref() else {
            self.set_status("S: untracked rows cannot be launched under workspace supervision");
            return;
        };
        let session_name = session.name.clone();
        let display_name = row.display_name.clone();
        let mpx_name = row.mpx_name.clone();
        let attached_clients = row.attached_clients;
        let state = row.state;
        let ownership = row.ownership;
        let logical_id = row.logical_id.clone();
        let receipt_backed = row
            .logical_id
            .as_ref()
            .is_some_and(|logical_id| self.has_supervision_evidence(logical_id));

        if receipt_backed
            && !matches!(
                ownership,
                RowOwnership::Owned
                    | RowOwnership::LegacyRestartRequired
                    | RowOwnership::SplitContainment
                    | RowOwnership::Pending
                    | RowOwnership::Ambiguous
                    | RowOwnership::Stale
            )
        {
            self.set_status("S: verifying the ownership receipt; wait for a final state");
            return;
        }

        match ownership {
            RowOwnership::Owned => {
                self.set_status(
                    "S: this row is already supervised; Enter attaches and r refreshes resources",
                );
                return;
            }
            RowOwnership::LegacyRestartRequired => {
                self.set_status(
                    "S: legacy v1 service is attach-only; exit it normally, then relaunch for v2 ownership",
                );
                return;
            }
            RowOwnership::SplitContainment => {
                self.set_status(
                    "S: containment is split by external descendants; whole-workload ownership is disabled",
                );
                return;
            }
            RowOwnership::Pending => {
                self.set_status(
                    "S: pending launch evidence blocks new supervision; inspect with `pa resources status`",
                );
                return;
            }
            RowOwnership::Ambiguous => {
                self.set_status(
                    "S: ownership evidence is ambiguous; no restart or resource action is allowed",
                );
                return;
            }
            RowOwnership::Stale => {
                if state != SessionState::NotStarted {
                    self.set_status(
                        "S: a stale receipt and a live ordinary target need separate cleanup/restart decisions",
                    );
                    return;
                }
                let Some(logical_id) = logical_id else {
                    self.set_status("S: stale row is missing its logical identity");
                    return;
                };
                let Some(receipt) = self.receipts.get(&logical_id).cloned() else {
                    self.set_status("S: stale row is missing its binding receipt");
                    return;
                };
                if let Err(error) = (self.supervision_preflight)() {
                    self.set_status(format!("S: {error:#}"));
                    return;
                }
                let limits = if receipt.limits.is_empty() {
                    ResourceLimits::defaults_for_kind(session.kind)
                } else {
                    receipt.limits.clone()
                };
                if let Err(error) =
                    self.open_supervise_limits_with(&session_name, limits, Some(receipt))
                {
                    self.set_status(format!("S: {error:#}"));
                }
                return;
            }
            RowOwnership::InvalidWorkspaceId => {
                self.set_status(
                    "S: workspace has an invalid ID; fix it explicitly before supervision",
                );
                return;
            }
            RowOwnership::Unsupported => {
                self.set_status("S: supervision is genuinely unsupported for this row");
                return;
            }
            RowOwnership::Unmanaged if state != SessionState::Live => {
                self.set_status("S: unmanaged row is not a live declared session");
                return;
            }
            RowOwnership::ExistingUnverified if state != SessionState::Live => {
                self.set_status("S: unverified row is not currently live");
                return;
            }
            RowOwnership::IdleSupported if state != SessionState::NotStarted => {
                self.set_status("S: supervisable row changed state; refresh and retry");
                return;
            }
            RowOwnership::NeedsWorkspaceId
            | RowOwnership::IdleSupported
            | RowOwnership::ExistingUnverified
            | RowOwnership::Unmanaged => {}
        }

        if !self.supervision_evidence_available {
            self.set_status("S: supervision evidence could not be loaded; creation is fail-closed");
            return;
        }
        if let Err(error) = (self.supervision_preflight)() {
            self.set_status(format!("S: {error:#}"));
            return;
        }

        if ownership == RowOwnership::IdleSupported {
            if let Err(error) = self.open_supervise_limits(&session_name) {
                self.set_status(format!("S: {error:#}"));
            }
            return;
        }

        self.pending = Some(PendingAction::PrepareSupervised {
            session_name,
            display_name,
            mpx_name,
            attached_clients,
            assign_workspace_id: ownership == RowOwnership::NeedsWorkspaceId,
            restart_live: state == SessionState::Live,
        });
    }

    fn open_supervise_limits(&mut self, session_name: &str) -> Result<()> {
        let kind = self
            .rows
            .iter()
            .find_map(|row| {
                row.session
                    .as_ref()
                    .filter(|session| session.name == session_name)
                    .map(|session| session.kind)
            })
            .flatten();
        self.open_supervise_limits_with(session_name, ResourceLimits::defaults_for_kind(kind), None)
    }

    fn open_supervise_limits_with(
        &mut self,
        session_name: &str,
        limits: ResourceLimits,
        stale_receipt: Option<BindingReceipt>,
    ) -> Result<()> {
        let index = self
            .rows
            .iter()
            .position(|row| {
                row.session
                    .as_ref()
                    .is_some_and(|session| session.name == session_name)
            })
            .ok_or_else(|| anyhow::anyhow!("declared session {session_name:?} disappeared"))?;
        let row = &self.rows[index];
        let expected_ownership = if stale_receipt.is_some() {
            RowOwnership::Stale
        } else {
            RowOwnership::IdleSupported
        };
        if row.state != SessionState::NotStarted || row.ownership != expected_ownership {
            anyhow::bail!(
                "session {session_name:?} is {}, not idle and supervisable",
                row.ownership.label()
            );
        }
        let mpx_name = row.mpx_name.clone();
        let (memory_high, memory_max, memory_swap_max, cpu_quota, tasks_max) =
            supervise_limit_fields(&limits);
        self.list_state.select(Some(index));
        self.supervising = Some(SuperviseState {
            stage: SuperviseStage::MemoryHigh,
            session_name: session_name.to_string(),
            mpx_name,
            memory_high,
            memory_max,
            memory_swap_max,
            cpu_quota,
            tasks_max,
            stale_receipt: stale_receipt.map(Box::new),
            error: None,
        });
        Ok(())
    }

    fn reload_workspace_checked(&mut self) -> Result<()> {
        let path = self
            .workspace
            .file_path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("workspace has no file on disk"))?;
        self.workspace = crate::config::load(&crate::config::LoadOptions {
            workspace_path: Some(path),
            ..Default::default()
        })?;
        let live = self.mux.list_sessions()?;
        self.rebuild_rows(&live);
        Ok(())
    }

    fn prepare_supervised_launch(
        &mut self,
        session_name: &str,
        expected_mpx_name: &str,
        assign_workspace_id: bool,
        restart_live: bool,
    ) -> Result<()> {
        (self.supervision_preflight)()?;

        if assign_workspace_id {
            let path = self
                .workspace
                .file_path
                .clone()
                .ok_or_else(|| anyhow::anyhow!("legacy workspace has no file to upgrade"))?;
            crate::config::ensure_workspace_id(&path)?;
        }
        self.reload_workspace_checked()?;

        let index = self
            .rows
            .iter()
            .position(|row| {
                row.session
                    .as_ref()
                    .is_some_and(|session| session.name == session_name)
            })
            .ok_or_else(|| anyhow::anyhow!("declared session {session_name:?} disappeared"))?;
        let row = &self.rows[index];
        if row.mpx_name != expected_mpx_name {
            anyhow::bail!(
                "session target changed from {expected_mpx_name:?} to {:?}; refusing to control it",
                row.mpx_name
            );
        }

        if row.state == SessionState::Live {
            if !restart_live {
                anyhow::bail!(
                    "session became live while preparing supervision; no process was stopped"
                );
            }
            if !matches!(
                row.ownership,
                RowOwnership::ExistingUnverified | RowOwnership::Unmanaged
            ) {
                anyhow::bail!(
                    "ownership changed to {}; refusing to stop the live target",
                    row.ownership.label()
                );
            }
            self.mux.kill(expected_mpx_name)?;
            let live = self.mux.list_sessions()?;
            self.rebuild_rows(&live);
        }

        if self.mux.has_session(expected_mpx_name)? {
            anyhow::bail!(
                "multiplexer target {expected_mpx_name:?} is still live; supervised launch was not opened"
            );
        }
        self.open_supervise_limits(session_name)?;
        Ok(())
    }

    fn validate_supervised_submission(&self, state: &SuperviseState) -> Result<()> {
        (self.supervision_preflight)()?;
        let row = self
            .rows
            .iter()
            .find(|row| {
                row.session
                    .as_ref()
                    .is_some_and(|session| session.name == state.session_name)
            })
            .ok_or_else(|| {
                anyhow::anyhow!("declared session {:?} disappeared", state.session_name)
            })?;
        let ownership_matches = match state.stale_receipt.as_deref() {
            Some(expected) => {
                row.ownership == RowOwnership::Stale
                    && row.logical_id.as_ref() == Some(&expected.logical_id)
                    && self.receipts.get(&expected.logical_id) == Some(expected)
            }
            None => row.ownership == RowOwnership::IdleSupported,
        };
        if row.mpx_name != state.mpx_name
            || row.state != SessionState::NotStarted
            || !ownership_matches
        {
            anyhow::bail!("session state changed before supervised launch; close and retry");
        }
        if self.mux.has_session(&state.mpx_name)? {
            anyhow::bail!(
                "multiplexer target {:?} became live before launch; Portagenty will not claim it",
                state.mpx_name
            );
        }
        Ok(())
    }

    fn handle_supervise_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<Action> {
        let mut state = self.supervising.take()?;
        match code {
            KeyCode::Esc => {
                self.set_status("supervised launch cancelled");
            }
            KeyCode::Enter | KeyCode::Tab => match state.stage {
                SuperviseStage::MemoryHigh => {
                    state.stage = SuperviseStage::MemoryMax;
                    state.error = None;
                    self.supervising = Some(state);
                }
                SuperviseStage::MemoryMax => {
                    state.stage = SuperviseStage::MemorySwapMax;
                    state.error = None;
                    self.supervising = Some(state);
                }
                SuperviseStage::MemorySwapMax => {
                    state.stage = SuperviseStage::CpuQuota;
                    state.error = None;
                    self.supervising = Some(state);
                }
                SuperviseStage::CpuQuota => {
                    state.stage = SuperviseStage::TasksMax;
                    state.error = None;
                    self.supervising = Some(state);
                }
                SuperviseStage::TasksMax => {
                    let parsed = (|| -> Result<ResourceLimits> {
                        ResourceLimits {
                            memory_high_bytes: if state.memory_high.trim().is_empty() {
                                None
                            } else {
                                Some(crate::supervision::model::parse_memory_size(
                                    state.memory_high.trim(),
                                )?)
                            },
                            memory_max_bytes: if state.memory_max.trim().is_empty() {
                                None
                            } else {
                                Some(crate::supervision::model::parse_memory_size(
                                    state.memory_max.trim(),
                                )?)
                            },
                            memory_swap_max_bytes: if state.memory_swap_max.trim().is_empty() {
                                None
                            } else {
                                Some(crate::supervision::model::parse_memory_size(
                                    state.memory_swap_max.trim(),
                                )?)
                            },
                            cpu_quota_percent: if state.cpu_quota.trim().is_empty() {
                                None
                            } else {
                                Some(crate::supervision::model::parse_cpu_quota(
                                    state.cpu_quota.trim(),
                                )?)
                            },
                            tasks_max: if state.tasks_max.trim().is_empty() {
                                None
                            } else {
                                Some(crate::supervision::model::parse_tasks_max(
                                    state.tasks_max.trim(),
                                )?)
                            },
                        }
                        .resolve_for_kind(
                            self.rows
                                .iter()
                                .find_map(|row| {
                                    row.session
                                        .as_ref()
                                        .filter(|session| session.name == state.session_name)
                                })
                                .and_then(|session| session.kind),
                        )
                    })();
                    match parsed {
                        Ok(limits) => match self.validate_supervised_submission(&state) {
                            Ok(()) => {
                                if let Some(receipt) = state.stale_receipt.take() {
                                    let session = self
                                        .rows
                                        .iter()
                                        .find_map(|row| {
                                            row.session.as_ref().filter(|session| {
                                                session.name == state.session_name
                                            })
                                        })
                                        .cloned()?;
                                    return Some(Action::LaunchStaleSupervised {
                                        session,
                                        receipt,
                                        limits,
                                    });
                                }
                                return Some(Action::LaunchSupervisedSelected(limits));
                            }
                            Err(error) => {
                                state.error = Some(format!("{error:#}"));
                                self.supervising = Some(state);
                            }
                        },
                        Err(error) => {
                            state.error = Some(format!("{error:#}"));
                            self.supervising = Some(state);
                        }
                    }
                }
            },
            KeyCode::BackTab => {
                state.stage = match state.stage {
                    SuperviseStage::MemoryHigh => SuperviseStage::MemoryHigh,
                    SuperviseStage::MemoryMax => SuperviseStage::MemoryHigh,
                    SuperviseStage::MemorySwapMax => SuperviseStage::MemoryMax,
                    SuperviseStage::CpuQuota => SuperviseStage::MemorySwapMax,
                    SuperviseStage::TasksMax => SuperviseStage::CpuQuota,
                };
                state.error = None;
                self.supervising = Some(state);
            }
            KeyCode::Backspace => {
                supervise_buffer(&mut state).pop();
                self.supervising = Some(state);
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                supervise_buffer(&mut state).clear();
                self.supervising = Some(state);
            }
            KeyCode::Char(_) if mods.contains(KeyModifiers::CONTROL) => {
                self.supervising = Some(state);
            }
            KeyCode::Char(character) => {
                supervise_buffer(&mut state).push(character);
                self.supervising = Some(state);
            }
            _ => {
                self.supervising = Some(state);
            }
        }
        None
    }

    /// Open the in-TUI edit overlay for the highlighted session.
    /// Untracked rows aren't editable (no workspace TOML entry to
    /// mutate); same for the synthetic live-browse workspace.
    /// Handle a key press while the add-session modal is open. Stage
    /// 1 = name, stage 2 = command. Enter advances name → command,
    /// then commits. Esc cancels. Standard input-editing keys apply.
    fn handle_add_session_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(mut st) = self.adding_session.take() else {
            return;
        };
        match code {
            KeyCode::Esc => {
                self.set_status("add cancelled");
                // modal closes (already took the Option).
            }
            KeyCode::Enter => match st.stage {
                AddStage::Name => {
                    if st.name.trim().is_empty() {
                        st.error = Some("name can't be empty".into());
                        self.adding_session = Some(st);
                    } else {
                        st.stage = AddStage::Command;
                        st.error = None;
                        self.adding_session = Some(st);
                    }
                }
                AddStage::Command => {
                    // Empty command → plain shell at the workspace
                    // dir. Matches what `pa init` scaffolds as its
                    // starter session. Most useful case: "just give
                    // me a persistent terminal here," no agent, no
                    // dev server.
                    let cmd_trimmed = st.command.trim();
                    let (command, kind): (&str, Option<crate::cli::AddKindArg>) =
                        if cmd_trimmed.is_empty() {
                            ("bash", Some(crate::cli::AddKindArg::Shell))
                        } else {
                            (cmd_trimmed, None)
                        };
                    if let Some(ws_path) = self.workspace.file_path.clone() {
                        match crate::cli::add(
                            st.name.trim(),
                            command,
                            None,
                            kind,
                            None,
                            Some(&ws_path),
                        ) {
                            Ok(()) => {
                                let name = st.name.clone();
                                let note = if cmd_trimmed.is_empty() {
                                    format!("added shell session {name:?}")
                                } else {
                                    format!("added session {name:?}")
                                };
                                self.set_status(note);
                                self.reload_workspace();
                                // modal closes.
                            }
                            Err(e) => {
                                st.error = Some(format!("{e:#}"));
                                self.adding_session = Some(st);
                            }
                        }
                    } else {
                        self.set_status("can't add to live-browse workspace");
                    }
                }
            },
            KeyCode::Tab => {
                // Tab from Name → Command (if name non-empty). Lets
                // users fill both fields without pressing Enter twice.
                if st.stage == AddStage::Name && !st.name.trim().is_empty() {
                    st.stage = AddStage::Command;
                    st.error = None;
                }
                self.adding_session = Some(st);
            }
            KeyCode::BackTab => {
                // Shift+Tab: go back to the previous stage.
                if st.stage == AddStage::Command {
                    st.stage = AddStage::Name;
                }
                self.adding_session = Some(st);
            }
            KeyCode::Backspace => {
                let buf = match st.stage {
                    AddStage::Name => &mut st.name,
                    AddStage::Command => &mut st.command,
                };
                buf.pop();
                self.adding_session = Some(st);
            }
            KeyCode::Char('h') if mods.contains(KeyModifiers::CONTROL) => {
                let buf = match st.stage {
                    AddStage::Name => &mut st.name,
                    AddStage::Command => &mut st.command,
                };
                buf.pop();
                self.adding_session = Some(st);
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                let buf = match st.stage {
                    AddStage::Name => &mut st.name,
                    AddStage::Command => &mut st.command,
                };
                buf.clear();
                self.adding_session = Some(st);
            }
            KeyCode::Char('w') if mods.contains(KeyModifiers::CONTROL) => {
                let buf = match st.stage {
                    AddStage::Name => &mut st.name,
                    AddStage::Command => &mut st.command,
                };
                while buf.ends_with(' ') {
                    buf.pop();
                }
                while buf.chars().last().is_some_and(|c| !c.is_whitespace()) {
                    buf.pop();
                }
                self.adding_session = Some(st);
            }
            KeyCode::Char(_) if mods.contains(KeyModifiers::CONTROL) => {
                // Eat stray Ctrl+<letter> so it doesn't hit the input.
                self.adding_session = Some(st);
            }
            KeyCode::Char(ch) => {
                let buf = match st.stage {
                    AddStage::Name => &mut st.name,
                    AddStage::Command => &mut st.command,
                };
                buf.push(ch);
                self.adding_session = Some(st);
            }
            _ => {
                self.adding_session = Some(st);
            }
        }
    }

    /// Reload the workspace from disk and rebuild the row list. Used
    /// after a successful add to reflect the new session in the TUI
    /// without requiring an Esc + re-entry.
    fn reload_workspace(&mut self) {
        if let Some(ws_path) = self.workspace.file_path.clone() {
            let opts = crate::config::LoadOptions {
                workspace_path: Some(ws_path),
                ..Default::default()
            };
            if let Ok(ws) = crate::config::load(&opts) {
                self.workspace = ws;
                let live = self.mux.list_sessions().unwrap_or_default();
                self.rebuild_rows(&live);
            }
        }
    }

    fn open_edit_overlay(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.session.is_none() {
            self.set_status("e: untracked rows aren't in the workspace TOML");
            return;
        }
        if self.workspace.file_path.is_none() {
            self.set_status("e: live-browse mode has no workspace file to edit");
            return;
        }
        self.editing = Some(crate::tui::edit::EditState::PickField);
    }

    /// Queue a switch-mpx confirm modal. Toggles tmux <-> zellij
    /// (the only two practical multiplexers); wezterm is a no-op
    /// with a status hint. Requires a workspace file on disk —
    /// the synthetic live-browse workspace can't be edited.
    fn open_switch_mpx_prompt(&mut self) {
        if self.workspace.file_path.is_none() {
            self.set_status("m: live-browse mode has no workspace file to edit");
            return;
        }
        use crate::domain::Multiplexer;
        let (from, to) = match self.workspace.multiplexer {
            Multiplexer::Tmux => (Multiplexer::Tmux, Multiplexer::Zellij),
            Multiplexer::Zellij => (Multiplexer::Zellij, Multiplexer::Tmux),
            Multiplexer::Wezterm => {
                self.set_status("m: wezterm isn't supported; nothing to switch to");
                return;
            }
        };
        let live_in_current = self
            .rows
            .iter()
            .filter(|r| r.state == SessionState::Live)
            .count();
        self.pending = Some(PendingAction::SwitchMpx {
            from,
            to,
            live_in_current,
        });
    }

    fn open_delete_prompt(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.session.is_none() {
            self.set_status("d: nothing to delete — untracked rows aren't in the workspace");
            return;
        }
        if self.workspace.file_path.is_none() {
            self.set_status("d: can't delete — this is the synthetic live-browse workspace");
            return;
        }
        self.pending = Some(PendingAction::DeleteSession {
            name: row.display_name.clone(),
        });
    }

    /// Execute a previously-queued action. Called only after the user
    /// confirmed via y/Y in the modal. Any error ends up in `status`
    /// so the user sees it without the modal re-opening.
    fn perform_pending(&mut self, action: PendingAction) {
        match action {
            PendingAction::KillSession {
                display_name,
                mpx_name,
                ..
            } => match self.mux.kill(&mpx_name) {
                Ok(()) => {
                    // Rebuild rows from the mpx's fresh view. The
                    // tracked row (if any) falls back to NotStarted;
                    // untracked rows vanish entirely.
                    let live = self.mux.list_sessions().unwrap_or_default();
                    self.rebuild_rows(&live);
                    self.set_status(format!("killed session {display_name:?}"));
                }
                Err(e) => {
                    self.set_status(format!("kill failed: {e:#}"));
                }
            },
            PendingAction::PrepareSupervised {
                session_name,
                display_name,
                mpx_name,
                assign_workspace_id,
                restart_live,
                ..
            } => match self.prepare_supervised_launch(
                &session_name,
                &mpx_name,
                assign_workspace_id,
                restart_live,
            ) {
                Ok(()) => {
                    if assign_workspace_id && restart_live {
                        self.set_status(format!(
                            "added a workspace ID and stopped {display_name:?}; choose supervised limits"
                        ));
                    } else if assign_workspace_id {
                        self.set_status("added a stable workspace ID; choose supervised limits");
                    } else {
                        self.set_status(format!(
                            "stopped {display_name:?}; choose supervised limits"
                        ));
                    }
                }
                Err(error) => {
                    self.set_status(format!("supervised restart preparation failed: {error:#}"));
                }
            },
            #[cfg(target_os = "linux")]
            PendingAction::StopOwned {
                display_name,
                receipt,
                force,
            } => {
                let outcome = (|| -> Result<crate::supervision::ActionResult> {
                    let backend = crate::supervision::LinuxSystemdBackend::connect()?;
                    match backend.reconcile(&receipt)? {
                        crate::supervision::OwnershipState::OwnedVerified(_) => {}
                        state => {
                            anyhow::bail!("ownership changed before control action: {state:?}")
                        }
                    }
                    if force {
                        backend.force_kill(&receipt)
                    } else {
                        if let Err(error) = crate::cli::graceful_stop_target(&receipt.mux_target) {
                            tracing::warn!(
                                target = "portagenty::tui",
                                error = %format!("{error:#}"),
                                "graceful multiplexer stop failed before systemd stop"
                            );
                        }
                        backend.stop_unit(&receipt)
                    }
                })();
                match outcome {
                    Ok(result) => {
                        if result.completed {
                            if let Ok(store) = crate::supervision::ReceiptStore::standard() {
                                let _ = store.remove(&receipt.logical_id);
                            }
                            self.receipts.remove(&receipt.logical_id);
                            self.resource_snapshots.remove(&receipt.logical_id);
                            self.reload_workspace();
                        }
                        self.set_status(format!("{display_name:?}: {}", result.final_state));
                    }
                    Err(error) => {
                        self.set_status(format!("resource control failed: {error:#}"));
                    }
                }
            }
            #[cfg(target_os = "linux")]
            PendingAction::RemoveStaleReceipt {
                display_name,
                receipt,
            } => {
                let outcome = (|| -> Result<()> {
                    let backend = crate::supervision::LinuxSystemdBackend::connect()?;
                    let store = crate::supervision::ReceiptStore::standard()?;
                    backend.remove_stale_binding(&store, &receipt)
                })();
                match outcome {
                    Ok(()) => {
                        self.receipts.remove(&receipt.logical_id);
                        self.resource_snapshots.remove(&receipt.logical_id);
                        self.resource_refresh_pending.remove(&receipt.logical_id);
                        let live = self.mux.list_sessions().unwrap_or_default();
                        self.rebuild_rows(&live);
                        self.set_status(format!(
                            "cleared dead receipt for {display_name:?}; no process was signalled"
                        ));
                    }
                    Err(error) => {
                        self.set_status(format!("stale receipt cleanup refused: {error:#}"));
                    }
                }
            }
            PendingAction::ReplaceStaleBinding { .. } => {
                self.set_status("stale replacement must be handed to the launch coordinator");
            }
            PendingAction::DeleteSession { name } => {
                let Some(path) = self.workspace.file_path.clone() else {
                    self.set_status("delete failed: no workspace file on disk");
                    return;
                };
                match crate::cli::remove_session_from_file(&path, &name) {
                    Ok(()) => {
                        // Drop from in-memory workspace + rebuild the
                        // row list so the TUI reflects the change
                        // immediately. The mpx session (if any)
                        // reappears as an Untracked row after rebuild.
                        self.workspace.sessions.retain(|s| s.name != name);
                        let live = self.mux.list_sessions().unwrap_or_default();
                        self.rebuild_rows(&live);
                        // Keep selection in-bounds.
                        if self.rows.is_empty() {
                            self.list_state.select(None);
                        } else {
                            let sel = self.list_state.selected().unwrap_or(0);
                            self.list_state.select(Some(sel.min(self.rows.len() - 1)));
                        }
                        self.set_status(format!("deleted session {name:?}"));
                    }
                    Err(e) => {
                        self.set_status(format!("delete failed: {e:#}"));
                    }
                }
            }
            PendingAction::SwitchMpx { to, .. } => {
                let Some(path) = self.workspace.file_path.clone() else {
                    self.set_status("switch-mpx failed: no workspace file on disk");
                    return;
                };
                match crate::workspace_edit::set_multiplexer(&path, to) {
                    Ok(()) => {
                        // The TUI's mux is the *old* mpx adapter
                        // (constructed in tui::run before we entered
                        // App::run); we can't safely swap it
                        // mid-loop because attached sessions would
                        // be left dangling. Instead, signal the
                        // user to back to the picker (Esc) and
                        // re-enter; on next entry the workspace
                        // file is re-read with the new mpx.
                        self.workspace.multiplexer = to;
                        self.set_status(format!(
                            "switched mpx to {to:?} — press Esc, then re-enter to use it"
                        ));
                    }
                    Err(e) => {
                        self.set_status(format!("switch-mpx failed: {e:#}"));
                    }
                }
            }
        }
    }

    /// Persist an edit op to the workspace TOML and reload the
    /// in-memory workspace + row list. Closes the edit overlay on
    /// success; leaves it open with a status hint on failure so the
    /// user can fix and retry without losing context.
    fn apply_edit_op(&mut self, op: crate::cli::EditOp) {
        let Some(path) = self.workspace.file_path.clone() else {
            self.set_status("edit failed: no workspace file on disk");
            return;
        };
        let Some(target) = self.selected_row().and_then(|r| r.session.clone()) else {
            self.set_status("edit failed: nothing selected");
            return;
        };
        let target_name = target.name.clone();
        match crate::cli::edit_session_in_file(&path, &target_name, &op) {
            Ok(()) => {
                // Reload the workspace from disk so name + cwd + env
                // changes flow into the resolved domain types
                // correctly (handles ~ / ${HOME} expansion etc.).
                match crate::config::load(&crate::config::LoadOptions {
                    workspace_path: Some(path),
                    ..Default::default()
                }) {
                    Ok(reloaded) => {
                        self.workspace = reloaded;
                        let live = self.mux.list_sessions().unwrap_or_default();
                        self.rebuild_rows(&live);
                        if !self.rows.is_empty() {
                            let sel = self.list_state.selected().unwrap_or(0);
                            self.list_state.select(Some(sel.min(self.rows.len() - 1)));
                        }
                        self.editing = None;
                        self.set_status(format!("edited session {target_name:?}"));
                    }
                    Err(e) => {
                        // The on-disk write succeeded but the
                        // reload failed — file is inconsistent.
                        // Surface the error and close the overlay
                        // so the user can investigate.
                        self.editing = None;
                        self.set_status(format!("edit wrote ok, reload failed: {e:#}"));
                    }
                }
            }
            Err(e) => {
                // Leave the overlay open so the user can correct
                // their input and retry without re-typing from
                // scratch — the state machine still has their last
                // input string.
                self.set_status(format!("edit failed: {e:#}"));
            }
        }
    }

    /// Apply a single key press, returning whatever [`Action`] it
    /// produced. Split from `handle_event` so tests drive input
    /// synchronously without faking a crossterm event stream.
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Action {
        // Help overlay: any key closes it, with a light special-case
        // so `?` toggles (press once to open, again to close rather
        // than being hot-swapped for an underlying-screen keystroke).
        if self.help_open {
            self.help_open = false;
            return Action::None;
        }
        if self.supervising.is_some() {
            return self
                .handle_supervise_key(code, mods)
                .unwrap_or(Action::None);
        }
        // Add-session modal: two-stage input (name → command). Enter
        // advances / commits, Esc cancels, Backspace & Ctrl+H delete,
        // Ctrl+U clears, Ctrl+<letter> is silently eaten.
        if self.adding_session.is_some() {
            self.handle_add_session_key(code, mods);
            return Action::None;
        }
        // General file-tree browsing overlay (session-list `t`).
        // Not tied to editing a session field — primary action from
        // here is `o` to drop to shell at the highlighted folder.
        if self.browsing.is_some() {
            let search = self.browsing.as_mut().unwrap();
            search.poll_background();
            search.tick_animation();
            use crate::tui::find::SearchOutcome;
            let result = crate::tui::find::handle_key(search, code, mods);
            match result {
                SearchOutcome::Continue => {}
                SearchOutcome::Cancel => {
                    self.browsing = None;
                }
                SearchOutcome::BackToSearch => {
                    if let Some(s) = self.browsing.as_mut() {
                        s.mode = crate::tui::find::FindMode::Search;
                    }
                }
                SearchOutcome::SearchFromHere(dir) => {
                    if let Some(s) = self.browsing.as_mut() {
                        s.mode = crate::tui::find::FindMode::Search;
                        s.set_root(dir);
                    }
                }
                SearchOutcome::OpenHelp => {
                    self.help_open = true;
                }
                SearchOutcome::OpenShellAt(dir) => {
                    self.browsing = None;
                    return Action::OpenShellAt(dir);
                }
                // ScaffoldAt / OpenExisting from inside the file-tree
                // browser don't make sense (we're already in a
                // workspace). Just close the overlay with a hint.
                SearchOutcome::ScaffoldAt(_) | SearchOutcome::OpenExisting(_) => {
                    self.browsing = None;
                    self.set_status(
                        "picking from the file tree here doesn't switch workspaces; \
                         use Esc → picker if that's what you want",
                    );
                }
            }
            return Action::None;
        }
        // CWD browse overlay: find overlay open for folder selection.
        if self.browsing_cwd.is_some() {
            let (ref session_name, ref mut search) = self.browsing_cwd.as_mut().unwrap();
            search.poll_background();
            search.tick_animation();
            use crate::tui::find::SearchOutcome;
            let result = crate::tui::find::handle_key(search, code, mods);
            // Extract the picked path (if any) before we drop the borrow.
            let picked_dir = match &result {
                SearchOutcome::ScaffoldAt(p) => Some(p.clone()),
                SearchOutcome::OpenExisting(p) => {
                    // p is a .portagenty.toml file; use its parent dir.
                    p.parent().map(|d| d.to_path_buf())
                }
                _ => None,
            };
            let sn = session_name.clone();
            match result {
                SearchOutcome::Continue => {}
                SearchOutcome::Cancel => {
                    self.browsing_cwd = None;
                    self.set_status("cwd browse cancelled");
                }
                SearchOutcome::BackToSearch => {
                    if let Some((_, s)) = self.browsing_cwd.as_mut() {
                        s.mode = crate::tui::find::FindMode::Search;
                    }
                }
                SearchOutcome::SearchFromHere(dir) => {
                    if let Some((_, s)) = self.browsing_cwd.as_mut() {
                        s.mode = crate::tui::find::FindMode::Search;
                        s.set_root(dir);
                    }
                }
                SearchOutcome::OpenShellAt(_) => {
                    // Shell-out from the cwd-browse overlay is
                    // ambiguous — we're mid-edit of a session field.
                    // Bounce back to search mode and show a hint.
                    if let Some((_, s)) = self.browsing_cwd.as_mut() {
                        s.mode = crate::tui::find::FindMode::Search;
                    }
                    self.set_status(
                        "o: use this from the session list (closes pa); \
                         here it cancels the cwd edit instead",
                    );
                }
                SearchOutcome::OpenHelp => {
                    self.help_open = true;
                }
                SearchOutcome::ScaffoldAt(_) | SearchOutcome::OpenExisting(_) => {
                    self.browsing_cwd = None;
                    if let Some(dir) = picked_dir {
                        let op = crate::cli::EditOp {
                            cwd: Some(dir.display().to_string()),
                            ..Default::default()
                        };
                        if let Some(ws_path) = self.workspace.file_path.clone() {
                            match crate::cli::edit_session_in_file(&ws_path, &sn, &op) {
                                Ok(()) => {
                                    if let Ok(reloaded) =
                                        crate::config::load(&crate::config::LoadOptions {
                                            workspace_path: Some(ws_path),
                                            ..Default::default()
                                        })
                                    {
                                        self.workspace = reloaded;
                                        let live = self.mux.list_sessions().unwrap_or_default();
                                        self.rebuild_rows(&live);
                                    }
                                    self.set_status(format!("cwd updated for {sn:?}"));
                                }
                                Err(e) => {
                                    self.set_status(format!("cwd update failed: {e:#}"));
                                }
                            }
                        }
                    }
                }
            }
            return Action::None;
        }
        // Edit overlay: divert keys to the edit module's state
        // machine. Apply outcomes go through cli::edit_session_in_file
        // (the same toml_edit-preserving helper the CLI uses) so
        // there's only one place that mutates the workspace TOML.
        if self.editing.is_some() {
            // Take ownership of the state for handle_key; put it back
            // unless the outcome closes the overlay.
            let mut state = self.editing.take().expect("editing was Some");
            let outcome = crate::tui::edit::handle_key(&mut state, code, mods);
            match outcome {
                crate::tui::edit::EditOutcome::Continue => {
                    self.editing = Some(state);
                }
                crate::tui::edit::EditOutcome::Cancel => {
                    self.set_status("edit cancelled");
                }
                crate::tui::edit::EditOutcome::Apply(op) => {
                    self.apply_edit_op(op);
                }
                crate::tui::edit::EditOutcome::BrowseForCwd => {
                    let session_name = self
                        .selected_row()
                        .map(|r| r.display_name.clone())
                        .unwrap_or_default();
                    self.browsing_cwd =
                        Some((session_name, crate::tui::find::SearchState::default()));
                }
            }
            return Action::None;
        }
        // Confirm modal: divert key handling until dismissed. y/Y
        // performs the pending action, anything else cancels.
        if let Some(action) = self.pending.take() {
            match crate::tui::confirm::classify(code) {
                crate::tui::confirm::ConfirmKey::Confirm => match action {
                    PendingAction::ReplaceStaleBinding {
                        session,
                        receipt,
                        limits,
                    } => {
                        return Action::LaunchStaleSupervised {
                            session,
                            receipt,
                            limits,
                        };
                    }
                    action => self.perform_pending(action),
                },
                crate::tui::confirm::ConfirmKey::Cancel => {
                    self.set_status("cancelled");
                }
            }
            return Action::None;
        }
        // Any keystroke clears a lingering status line.
        self.clear_status();
        match (code, mods) {
            (KeyCode::Char('?'), _) => {
                self.help_open = true;
                Action::None
            }
            // Ctrl+D half-page jump — must come BEFORE bare `d`
            // (delete) since `_` matches any modifier.
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                for _ in 0..5 {
                    self.select_next();
                }
                Action::None
            }
            (KeyCode::Char('d'), _) => {
                self.open_delete_prompt();
                Action::None
            }
            (KeyCode::Char('x'), _) => {
                self.open_kill_prompt();
                Action::None
            }
            (KeyCode::Char('X'), _) => {
                self.open_force_kill_prompt();
                Action::None
            }
            (KeyCode::Char('S'), _) => {
                self.open_supervise_modal();
                Action::None
            }
            (KeyCode::Char('r'), _) => {
                #[cfg(target_os = "linux")]
                self.request_selected_resource_refresh();
                #[cfg(not(target_os = "linux"))]
                self.set_status("resource refresh is unsupported on this platform");
                Action::None
            }
            (KeyCode::Char('m'), _) => {
                self.open_switch_mpx_prompt();
                Action::None
            }
            (KeyCode::Char('e'), _) => {
                self.open_edit_overlay();
                Action::None
            }
            // `a` opens the "add new session" modal (two-stage name
            // → command input). Writes via cli::add_session_to_file,
            // same path the CLI uses.
            (KeyCode::Char('a'), _) => {
                self.adding_session = Some(AddSessionState {
                    stage: AddStage::Name,
                    name: String::new(),
                    command: String::new(),
                    error: None,
                });
                Action::None
            }
            // `o` → open the workspace's dir in a plain terminal,
            // outside of pa. No mpx, no session — just `cd <dir> && $SHELL`.
            (KeyCode::Char('o'), _) => Action::OpenShellAt(self.workspace_dir()),
            // `t` → open the file tree rooted at the workspace's dir.
            // Browse around, `o` inside to shell-out at any folder.
            (KeyCode::Char('t'), _) => {
                self.open_file_tree();
                Action::None
            }
            // `z` → toggle expand-on-select. When on (default), the
            // highlighted row shows full desc/cmd/cwd; off is max-
            // density one-line scanning.
            (KeyCode::Char('z'), _) => {
                self.expand_selected = !self.expand_selected;
                self.set_status(if self.expand_selected {
                    "expand-on-select: on"
                } else {
                    "expand-on-select: off"
                });
                Action::None
            }
            // `q` in the session list closes this view and goes back
            // to the workspace picker (home screen). `Ctrl+Q` matches
            // for symmetry. `Ctrl+C` still hard-quits the app for the
            // "I really want out" case.
            (KeyCode::Char('q'), _) => Action::Back,
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                Action::Quit
            }
            // Esc dismisses the status line first if one is showing,
            // otherwise backs out to the picker. Two-stage Esc means
            // a stray dismiss never throws the user back to the
            // home screen by accident.
            (KeyCode::Esc, _) => {
                if self.status.is_some() {
                    self.clear_status();
                    Action::None
                } else {
                    Action::Back
                }
            }
            (KeyCode::Enter, _) => Action::LaunchSelected,
            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                self.select_next();
                Action::None
            }
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                self.select_prev();
                Action::None
            }
            (KeyCode::Char('g'), _) | (KeyCode::Home, _) => {
                self.select_first();
                Action::None
            }
            (KeyCode::Char('G'), _) | (KeyCode::End, _) => {
                self.select_last();
                Action::None
            }
            // Ctrl+U: half-page up (vim-style). Ctrl+D is earlier.
            (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                for _ in 0..5 {
                    self.select_prev();
                }
                Action::None
            }
            // PageDown / PageUp.
            (KeyCode::PageDown, _) => {
                for _ in 0..10 {
                    self.select_next();
                }
                Action::None
            }
            (KeyCode::PageUp, _) => {
                for _ in 0..10 {
                    self.select_prev();
                }
                Action::None
            }
            // `l` / Right → launch selected (vim-style drill-in).
            (KeyCode::Char('l'), _) | (KeyCode::Right, _) => Action::LaunchSelected,
            _ => Action::None,
        }
    }

    /// Render a single frame. Pulled out so tests can call it against
    /// a `TestBackend` without needing the event loop.
    pub fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();

        // Column header is only useful at widths where we render
        // columns; narrow "card" mode has no columns to label.
        let show_col_header = area.width >= 60;
        let header_h: u16 = if show_col_header { 1 } else { 0 };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),        // title
                Constraint::Length(header_h), // column header
                Constraint::Min(0),           // list
                Constraint::Length(1),        // footer line 1
                Constraint::Length(1),        // footer line 2
            ])
            .split(area);

        let tracked = self.workspace.sessions.len();
        let untracked = self
            .rows
            .iter()
            .filter(|r| r.state == SessionState::Untracked)
            .count();

        // Mpx badge: distinct accent color per multiplexer so the
        // user can tell at a glance which backend they're talking to.
        // Useful when juggling a zellij workspace for some projects
        // and a tmux one for others on the same machine.
        let (mpx_label, mpx_color) = match self.workspace.multiplexer {
            crate::domain::Multiplexer::Tmux => ("tmux", Color::Cyan),
            crate::domain::Multiplexer::Zellij => ("zellij", Color::Magenta),
            crate::domain::Multiplexer::Wezterm => ("wezterm", Color::LightYellow),
        };
        let mut title_spans: Vec<Span<'static>> = vec![
            Span::raw(" "),
            Span::styled(
                self.workspace.name.clone(),
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ),
            Span::raw("  "),
            Span::styled(
                format!("[{mpx_label}]"),
                Style::default().fg(mpx_color).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{tracked} session{}", if tracked == 1 { "" } else { "s" }),
                Style::default().add_modifier(Modifier::REVERSED),
            ),
        ];
        if untracked > 0 {
            title_spans.push(Span::raw("  "));
            title_spans.push(Span::styled(
                format!("· {untracked} untracked "),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::REVERSED),
            ));
        } else {
            title_spans.push(Span::styled(
                " ".to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(title_spans))
                .style(Style::default().add_modifier(Modifier::REVERSED)),
            chunks[0],
        );

        if show_col_header {
            let col_header = column_header_line(area.width);
            frame.render_widget(
                Paragraph::new(col_header).style(Style::default().add_modifier(Modifier::DIM)),
                chunks[1],
            );
        }

        self.render_session_list(frame, chunks[2]);

        // Status line preempts the keybind footer when set. Auto-
        // clears via STATUS_TTL or on Esc.
        if let Some(status) = &self.status {
            let line = Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    status.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    "(Esc dismisses)",
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]);
            frame.render_widget(Paragraph::new(line), chunks[3]);
            // Empty second footer line when status is showing.
            frame.render_widget(Paragraph::new(""), chunks[4]);
        } else {
            // 2-line footer. Line 1: primary keys. Line 2: actions.
            use crate::tui::footer::Entry;
            crate::tui::footer::render(
                frame,
                chunks[3],
                &[
                    Entry::new("Esc/q", "back"),
                    Entry::new("?", "help"),
                    Entry::new("Enter/l", "attach/start"),
                    Entry::new("j/k", "nav"),
                    Entry::new("g/G", "top/btm"),
                ],
            );
            let sep = Style::default().fg(Color::DarkGray);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" ─── ", sep),
                    Span::styled(
                        "a ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("add  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "t ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("tree  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "o ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("shell  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "e ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("edit  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "d ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("delete  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "x ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("stop  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "S ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("supervise  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "r ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("resources  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "z ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("fold  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "m ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("mpx", Style::default().add_modifier(Modifier::DIM)),
                ])),
                chunks[4],
            );
        }

        // Confirm modal above content, below help (help wins if both
        // somehow open; in practice they're mutually exclusive).
        if let Some(pending) = &self.pending {
            let (title, body) = confirm_copy(pending, &self.workspace.name);
            crate::tui::confirm::render(frame, area, &title, &body);
        }

        // Edit overlay also above content; help still wins above this.
        if let Some(state) = &self.editing {
            let session_name = self
                .selected_row()
                .map(|r| r.display_name.clone())
                .unwrap_or_default();
            crate::tui::edit::render(frame, area, &session_name, state);
        }

        // CWD browse overlay — same find overlay as the picker's `n`.
        if let Some((_, ref mut search)) = self.browsing_cwd {
            crate::tui::find::render(frame, area, search);
        }

        // General file-tree browse overlay (session-list `t`).
        if let Some(ref mut search) = self.browsing {
            crate::tui::find::render(frame, area, search);
        }

        // Add-session modal: above content, under help.
        if let Some(st) = &self.adding_session {
            render_add_session_modal(frame, area, st);
        }
        if let Some(state) = &self.supervising {
            render_supervise_modal(frame, area, state);
        }

        // Help overlay renders last so it sits on top of everything.
        if self.help_open {
            crate::tui::help::render_overlay(
                frame,
                area,
                crate::tui::help::HelpContext::SessionList,
            );
        }
    }

    fn render_session_list(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if self.rows.is_empty() {
            let empty = Paragraph::new(" No sessions defined or running. ")
                .style(Style::default().add_modifier(Modifier::DIM));
            frame.render_widget(empty, area);
            return;
        }

        let width = area.width;
        // Compute column widths once, shared across all rows so the
        // table is actually aligned. Name column caps at 20; cwd and
        // command get proportional budgets based on remaining width.
        let name_col = self
            .rows
            .iter()
            .map(|r| r.display_name.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(4, 20);

        // Fixed overhead:
        //   2 highlight symbol, 1 gutter, 1 marker, 1 sep,
        //   0–2 kind glyph, 1 sep, name_col, 2 sep, status (~11),
        //   2 sep padding for safety.
        let kind_space = if self.rows.iter().any(|r| kind_glyph_present(r.kind)) {
            2
        } else {
            0
        };
        let fixed = 2 + 1 + 1 + 1 + kind_space + 1 + name_col + 2 + 11 + 2;
        let remaining = (width as usize).saturating_sub(fixed);
        // Split remaining between cwd and command roughly 55/45.
        let cwd_col = (remaining * 55 / 100).min(40);
        let cmd_col = remaining.saturating_sub(cwd_col + 2);

        // Only the highlighted row expands (when the feature is on),
        // so at most one row is ever multi-line beyond its base form.
        let selected = self.list_state.selected();
        let expand = self.expand_selected;
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let expanded = expand && Some(i) == selected;
                row_list_item(
                    r,
                    name_col,
                    width,
                    cwd_col,
                    cmd_col,
                    kind_space > 0,
                    expanded,
                )
            })
            .collect();

        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }
}

#[allow(clippy::too_many_arguments)]
fn row_list_item(
    row: &SessionRow,
    name_col: usize,
    width: u16,
    cwd_col: usize,
    cmd_col: usize,
    reserve_kind_space: bool,
    expanded: bool,
) -> ListItem<'static> {
    // State marker (● ○ ?) — color encodes Live/NotStarted/Untracked.
    // The session name picks up the same hue (not full color) so the
    // row reads at a glance without needing the marker.
    let marker_style = match row.state {
        SessionState::Live => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        SessionState::NotStarted => Style::default().add_modifier(Modifier::DIM),
        SessionState::Untracked => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    };
    let name_style = match row.state {
        SessionState::Live => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        SessionState::NotStarted => Style::default().add_modifier(Modifier::BOLD),
        SessionState::Untracked => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    };

    // Kind marker — small per-kind glyph shown right after the state
    // marker when the session has a kind hint.
    let (kind_glyph, kind_style) = kind_display(row.kind);

    // Status tag: includes attached-client count when the mpx reports
    // it (tmux does; zellij doesn't expose per-session, so no count).
    let state_status = match row.state {
        SessionState::Live => {
            if let Some(n) = row.attached_clients {
                if n > 1 {
                    format!("live · {n} clients")
                } else if n == 1 {
                    "live · 1 client".to_string()
                } else {
                    "live · detached".to_string()
                }
            } else {
                row.state.label().to_string()
            }
        }
        _ => row.state.label().to_string(),
    };
    let status_label = format!("[{state_status} · {}]", row.ownership.label());

    // Human description note, when the session declared one. Only
    // tracked rows carry a `session`; untracked rows never have one.
    // Empty descriptions are treated as absent.
    let description = row
        .session
        .as_ref()
        .and_then(|s| s.description.as_deref())
        .filter(|d| !d.is_empty());

    // Narrow: render each row as a two-line "card". Line 1 is the
    // essentials (marker + name + status tag). Line 2 is a dim,
    // indented detail line. When the session has a description it
    // takes the detail line (it's the human "what is this"); else we
    // fall back to the technical `command · path`.
    if width < 60 {
        let line1 = {
            let mut s: Vec<Span<'static>> = Vec::with_capacity(8);
            s.push(Span::raw(" "));
            s.push(Span::styled(row.state.marker().to_string(), marker_style));
            s.push(Span::raw(" "));
            if let Some(glyph) = kind_glyph {
                s.push(Span::styled(glyph.to_string(), kind_style));
                s.push(Span::raw(" "));
            } else if reserve_kind_space {
                s.push(Span::raw("  "));
            }
            s.push(Span::styled(row.display_name.clone(), name_style));
            s.push(Span::raw("  "));
            s.push(Span::styled(
                status_label.clone(),
                Style::default().add_modifier(Modifier::DIM),
            ));
            Line::from(s)
        };
        // Detail line: indent under the name. A description wins the
        // line (dim italic); otherwise show "cmd · path" with
        // tolerable middle-truncation so it always fits the width.
        let detail_budget = (width as usize).saturating_sub(6).max(10);
        let (raw_detail, detail_style) = match description {
            Some(d) => (
                d.to_string(),
                Style::default()
                    .add_modifier(Modifier::DIM)
                    .add_modifier(Modifier::ITALIC),
            ),
            None => {
                let cmd = row.command_display.clone();
                let path = compact_path(&row.cwd_display);
                let text = if cmd == "(unknown)" {
                    path
                } else if path == "(unknown)" || path.is_empty() {
                    cmd
                } else {
                    format!("{cmd}  ·  {path}")
                };
                (text, Style::default().add_modifier(Modifier::DIM))
            }
        };
        let detail = pad_or_truncate(&raw_detail, detail_budget);
        let line2 = Line::from(vec![Span::raw("    "), Span::styled(detail, detail_style)]);
        let mut lines = vec![line1, line2];
        if expanded {
            lines.extend(expansion_lines(row, description, width));
        }
        return ListItem::new(lines);
    }

    // Wide: single-line aligned table matching the column header.
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(12);
    spans.push(Span::raw(" "));
    spans.push(Span::styled(row.state.marker().to_string(), marker_style));
    spans.push(Span::raw(" "));
    if let Some(glyph) = kind_glyph {
        spans.push(Span::styled(glyph.to_string(), kind_style));
        spans.push(Span::raw(" "));
    } else if reserve_kind_space {
        spans.push(Span::raw("  "));
    }
    let name_cell = pad_or_truncate(&row.display_name, name_col);
    spans.push(Span::styled(name_cell, name_style));
    // The detail column shows the description when present (dim
    // italic — it's the human note), else the raw command (dim). Same
    // fixed-width cell either way, so columns stay aligned whether or
    // not a row is annotated.
    let (detail_text, detail_style) = match description {
        Some(d) => (
            d.to_string(),
            Style::default()
                .add_modifier(Modifier::DIM)
                .add_modifier(Modifier::ITALIC),
        ),
        None => (
            row.command_display.clone(),
            Style::default().add_modifier(Modifier::DIM),
        ),
    };
    if width >= 80 && cwd_col >= 8 {
        spans.push(Span::raw("  "));
        let cwd_cell = pad_or_truncate(&compact_path(&row.cwd_display), cwd_col);
        spans.push(Span::raw(cwd_cell));
        spans.push(Span::raw("  "));
        let cmd_cell = pad_or_truncate(&detail_text, cmd_col.max(4));
        spans.push(Span::styled(cmd_cell, detail_style));
    } else {
        // 60..80: no cwd column; detail and status only.
        spans.push(Span::raw("  "));
        let cmd_cell = pad_or_truncate(&detail_text, 24);
        spans.push(Span::styled(cmd_cell, detail_style));
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        status_label.clone(),
        Style::default().add_modifier(Modifier::DIM),
    ));
    // Relative-time hint (e.g. "2h ago") on wide rows. Only populated
    // for Live state; other states get blank padding so the column
    // stays aligned.
    if width >= 80 {
        spans.push(Span::raw("  "));
        let rel = match row.state {
            SessionState::Live => crate::state::relative_time(row.last_attached_unix),
            _ => String::new(),
        };
        spans.push(Span::styled(
            pad_or_truncate(&rel, 10),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    let mut lines = vec![Line::from(spans)];
    if expanded {
        lines.extend(expansion_lines(row, description, width));
    }
    ListItem::new(lines)
}

/// Detail lines appended beneath the highlighted row when
/// expand-on-select is on. Reveals the full (wrapped) description,
/// the REAL command — which an annotated row's COMMAND cell hides —
/// and the cwd, each behind a dim left-gutter label (`desc ▸` /
/// `cmd ▸` / `cwd ▸`). Only ever rendered on the selected row, which
/// always carries the highlight background, so the description text
/// is left un-dimmed for legibility on that saturated background;
/// the technical cmd/cwd stay dim. Bounded: the description is capped
/// at 3 wrapped lines so a short terminal can't clip a tall item.
fn expansion_lines(row: &SessionRow, description: Option<&str>, width: u16) -> Vec<Line<'static>> {
    // 5 indent + 4 label + " ▸ " = 12 cols before the value starts.
    const VALUE_INDENT: usize = 12;
    const MAX_DESC_LINES: usize = 3;
    let value_w = (width as usize).saturating_sub(VALUE_INDENT).max(8);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let plain = Style::default();
    let mut lines: Vec<Line<'static>> = Vec::new();

    if let Some(desc) = description {
        let wrapped = crate::tui::confirm::wrap_to_width(desc, value_w);
        let shown = wrapped.len().min(MAX_DESC_LINES);
        for (i, chunk) in wrapped.iter().take(shown).enumerate() {
            let mut text = chunk.clone();
            // Mark truncation if the description ran past the cap.
            if i + 1 == shown && wrapped.len() > MAX_DESC_LINES {
                text = clip_end(&format!("{text} …"), value_w);
            }
            if i == 0 {
                lines.push(labeled_detail_line("desc", text, plain));
            } else {
                lines.push(detail_continuation_line(text, plain));
            }
        }
    }
    // The real command — the one an annotated COMMAND cell overrides.
    lines.push(labeled_detail_line(
        "cmd",
        clip_end(&row.command_display, value_w),
        dim,
    ));
    lines.push(labeled_detail_line(
        "cwd",
        clip_end(&compact_path(&row.cwd_display), value_w),
        dim,
    ));
    if let Some(summary) = &row.resource_summary {
        lines.push(labeled_detail_line(
            "res",
            clip_end(summary, value_w),
            plain,
        ));
    }
    for detail in row.resource_details.iter().take(3) {
        lines.push(detail_continuation_line(clip_end(detail, value_w), dim));
    }
    lines
}

/// First line of a labeled detail field: `     desc ▸ <value>`.
fn labeled_detail_line(label: &str, value: String, value_style: Style) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    Line::from(vec![
        Span::raw("     "),
        Span::styled(format!("{label:<4}"), dim),
        Span::styled(" ▸ ", dim),
        Span::styled(value, value_style),
    ])
}

/// Wrapped-description continuation line, aligned under the value.
fn detail_continuation_line(value: String, value_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::raw("            "), // 12 spaces = VALUE_INDENT
        Span::styled(value, value_style),
    ])
}

/// Truncate `s` to `width` chars, appending `…` when it overflows.
/// End-truncation (not middle) — cheap and fine for detail values.
fn clip_end(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        return s.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let mut out: String = s.chars().take(width - 1).collect();
    out.push('…');
    out
}

#[cfg(target_os = "linux")]
fn resource_summary(snapshot: &ResourceSnapshot) -> String {
    let cpu = match &snapshot.cpu_percent {
        MetricValue::Value(value) => format!("CPU {value:.0}%"),
        _ => "CPU --".into(),
    };
    let memory = match &snapshot.memory_current_bytes {
        MetricValue::Value(value) => format!("mem {}", compact_bytes(*value)),
        _ => "mem --".into(),
    };
    let swap = match &snapshot.swap_current_bytes {
        MetricValue::Value(value) => format!("swap {}", compact_bytes(*value)),
        _ => "swap --".into(),
    };
    let tasks = match &snapshot.tasks_current {
        MetricValue::Value(value) => format!("tasks {value}"),
        _ => "tasks --".into(),
    };
    format!("{cpu} · {memory} · {swap} · {tasks}")
}

#[cfg(target_os = "linux")]
fn resource_details(snapshot: &ResourceSnapshot) -> Vec<String> {
    let mut details = Vec::new();
    if let MetricValue::Value(events) = &snapshot.memory_events {
        details.push(format!(
            "memory events: high={} oom={} oom_kill={}",
            events.get("high").copied().unwrap_or(0),
            events.get("oom").copied().unwrap_or(0),
            events.get("oom_kill").copied().unwrap_or(0)
        ));
    }
    if let MetricValue::Value(pressure) = &snapshot.memory_pressure {
        if let Some(some) = &pressure.some {
            details.push(format!(
                "memory PSI some: avg10={:.2} avg60={:.2}",
                some.avg10, some.avg60
            ));
        }
    }
    if let (MetricValue::Value(read), MetricValue::Value(write)) = (
        &snapshot.io_read_bytes_per_sec,
        &snapshot.io_write_bytes_per_sec,
    ) {
        details.push(format!(
            "I/O: {}/s read · {}/s write",
            compact_bytes(*read as u64),
            compact_bytes(*write as u64)
        ));
    }
    details
}

#[cfg(target_os = "linux")]
fn resource_event_notice(
    previous: Option<&ResourceSnapshot>,
    current: &ResourceSnapshot,
) -> Option<String> {
    let previous = previous?;
    let mut events = Vec::new();
    if let (MetricValue::Value(old), MetricValue::Value(new)) =
        (&previous.memory_events, &current.memory_events)
    {
        for (key, label) in [
            ("high", "MemoryHigh"),
            ("oom", "OOM"),
            ("oom_kill", "OOM kill"),
        ] {
            let old = old.get(key).copied().unwrap_or(0);
            let new = new.get(key).copied().unwrap_or(0);
            if new > old {
                events.push(format!("{label} +{}", new - old));
            }
        }
    }
    if let (MetricValue::Value(old), MetricValue::Value(new)) =
        (&previous.tasks_events, &current.tasks_events)
    {
        let old = old.get("max").copied().unwrap_or(0);
        let new = new.get("max").copied().unwrap_or(0);
        if new > old {
            events.push(format!("TasksMax +{}", new - old));
        }
    }
    if events.is_empty() {
        None
    } else {
        Some(events.join(", "))
    }
}

fn compact_bytes(value: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let value = value as f64;
    if value >= GIB {
        format!("{:.1}G", value / GIB)
    } else if value >= MIB {
        format!("{:.0}M", value / MIB)
    } else if value >= KIB {
        format!("{:.0}K", value / KIB)
    } else {
        format!("{value:.0}B")
    }
}

fn supervise_buffer(state: &mut SuperviseState) -> &mut String {
    match state.stage {
        SuperviseStage::MemoryHigh => &mut state.memory_high,
        SuperviseStage::MemoryMax => &mut state.memory_max,
        SuperviseStage::MemorySwapMax => &mut state.memory_swap_max,
        SuperviseStage::CpuQuota => &mut state.cpu_quota,
        SuperviseStage::TasksMax => &mut state.tasks_max,
    }
}

fn render_supervise_modal(frame: &mut Frame<'_>, area: Rect, state: &SuperviseState) {
    use ratatui::widgets::{Block, Borders, Clear};
    let overlay_w = area.width.saturating_sub(4).clamp(42, 74);
    let overlay_h = if state.error.is_some() { 12 } else { 11 }.min(area.height.saturating_sub(2));
    let region = Rect {
        x: area.x + (area.width.saturating_sub(overlay_w)) / 2,
        y: area.y + (area.height.saturating_sub(overlay_h)) / 2,
        width: overlay_w,
        height: overlay_h,
    };
    frame.render_widget(Clear, region);
    let active = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let style = |stage| if state.stage == stage { active } else { dim };
    let caret = Span::styled(
        "_",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::SLOW_BLINK),
    );
    let field = |label: &'static str, value: &str, stage| {
        let mut spans = vec![
            Span::styled(format!("  {label:<13}"), style(stage)),
            Span::styled(value.to_string(), style(stage)),
        ];
        if state.stage == stage {
            spans.push(caret.clone());
        }
        Line::from(spans)
    };
    let mut lines = vec![
        Line::raw(""),
        field(
            "Memory High",
            &state.memory_high,
            SuperviseStage::MemoryHigh,
        ),
        Line::styled(
            "    MemoryHigh is a reclaim threshold, not a hard ceiling",
            dim,
        ),
        field("Memory max", &state.memory_max, SuperviseStage::MemoryMax),
        field(
            "Swap max",
            &state.memory_swap_max,
            SuperviseStage::MemorySwapMax,
        ),
        field("CPU quota", &state.cpu_quota, SuperviseStage::CpuQuota),
        Line::styled("    800% permits up to eight CPU cores", dim),
        field("Tasks max", &state.tasks_max, SuperviseStage::TasksMax),
        Line::styled("    maximum tasks/threads", dim),
    ];
    if let Some(error) = &state.error {
        lines.push(Line::styled(
            format!("  {error}"),
            Style::default().fg(Color::Red),
        ));
    }
    lines.push(Line::styled(
        "  Enter/Tab next · Ctrl+U clears/unsets field · Enter on Tasks Max launches · Esc cancel",
        dim,
    ));
    let block = Block::default()
        .title(" Supervised launch ")
        .title_style(active)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(Paragraph::new(lines).block(block), region);
}

/// Centered two-field input modal for adding a new session. Stage
/// 1 = name, stage 2 = command. Active field shows a bold prompt;
/// the inactive one is dim. Error (if any) renders in red between
/// the fields and the help line.
fn render_add_session_modal(frame: &mut Frame<'_>, area: Rect, st: &AddSessionState) {
    use ratatui::widgets::{Block, Borders, Clear};
    let w = area.width;
    let h = area.height;
    let overlay_w = w.saturating_sub(4).clamp(40, 72);
    let overlay_h: u16 = if st.error.is_some() { 10 } else { 9 };
    let overlay_h = overlay_h.min(h.saturating_sub(2));
    let x = area.x + (w.saturating_sub(overlay_w)) / 2;
    let y = area.y + (h.saturating_sub(overlay_h)) / 2;
    let region = Rect {
        x,
        y,
        width: overlay_w,
        height: overlay_h,
    };
    frame.render_widget(Clear, region);

    let block = Block::default()
        .title(" Add session ")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let caret = Span::styled(
        "_",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::SLOW_BLINK),
    );
    let active = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);

    let (name_style, cmd_style) = match st.stage {
        AddStage::Name => (active, dim),
        AddStage::Command => (dim, active),
    };
    // Placeholder in the command field when stage=Command and
    // input is empty. Signals "press Enter now for a plain shell
    // session" without forcing the user to type "bash".
    let cmd_empty = st.stage == AddStage::Command && st.command.is_empty();
    let cmd_placeholder = Span::styled(
        "(empty → plain shell)",
        Style::default()
            .add_modifier(Modifier::DIM)
            .fg(Color::DarkGray),
    );
    let mut lines = vec![
        Line::from(vec![
            Span::styled("  name:    ", name_style),
            Span::styled(
                st.name.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            if st.stage == AddStage::Name {
                caret.clone()
            } else {
                Span::raw("")
            },
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  command: ", cmd_style),
            Span::styled(
                st.command.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            if cmd_empty {
                cmd_placeholder
            } else {
                Span::raw("")
            },
            if st.stage == AddStage::Command {
                caret.clone()
            } else {
                Span::raw("")
            },
        ]),
    ];

    if let Some(err) = &st.error {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(err.to_string(), Style::default().fg(Color::Red)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Tab next · Enter confirm · Esc cancel",
        Style::default().add_modifier(Modifier::DIM),
    )));

    frame.render_widget(Paragraph::new(lines).block(block), region);
}

/// Title + body strings for a pending confirm modal. Kept next to
/// the PendingAction enum so both evolve together when new actions
/// are added.
fn confirm_copy(pending: &PendingAction, workspace_name: &str) -> (String, String) {
    match pending {
        PendingAction::DeleteSession { name } => (
            "Delete session".into(),
            format!(
                "Remove session {name:?} from workspace {workspace_name:?}? \
                 This edits the workspace TOML; any running mpx session with this \
                 name stays alive (it'll reappear as an Untracked row)."
            ),
        ),
        PendingAction::KillSession {
            display_name,
            attached_clients,
            ..
        } => {
            let extra = match attached_clients {
                Some(n) if *n >= 2 => {
                    format!(" {n} clients are currently attached — they will all be disconnected.")
                }
                Some(1) => " 1 client is currently attached — it will be disconnected.".into(),
                _ => String::new(),
            };
            (
                "Kill session".into(),
                format!(
                    "Terminate the live mpx session {display_name:?}?{extra} \
                     This does NOT edit the workspace file, so the declared \
                     session will reappear as idle on the next refresh."
                ),
            )
        }
        PendingAction::PrepareSupervised {
            display_name,
            attached_clients,
            assign_workspace_id,
            restart_live,
            ..
        } => {
            let clients = match attached_clients {
                Some(n) if *n >= 2 => {
                    format!(" {n} clients are attached and will all be disconnected.")
                }
                Some(1) => " 1 client is attached and will be disconnected.".into(),
                _ => String::new(),
            };
            let identity = if *assign_workspace_id {
                " Portagenty will first add a stable UUID to the workspace file."
            } else {
                ""
            };
            if *restart_live {
                (
                    "Restart under supervision".into(),
                    format!(
                        "Restart {display_name:?} under supervision?{identity}{clients} The exact existing multiplexer session will be terminated, then a fresh private target will be launched after you choose limits. The running process tree is not migrated or claimed."
                    ),
                )
            } else {
                (
                    "Enable workspace supervision".into(),
                    format!(
                        "Add a stable UUID to workspace {workspace_name:?}, then choose supervised limits for {display_name:?}? Existing workspace comments and sessions stay intact."
                    ),
                )
            }
        }
        #[cfg(target_os = "linux")]
        PendingAction::StopOwned {
            display_name,
            force,
            ..
        } => {
            if *force {
                (
                    "Force-kill owned workload".into(),
                    format!(
                        "Send SIGKILL to every process in the verified control group for {display_name:?}? \
                         This is a separate irreversible escalation and can lose unsaved work."
                    ),
                )
            } else {
                (
                    "Stop owned workload".into(),
                    format!(
                        "Gracefully close the exact multiplexer target for {display_name:?}, then request \
                         a non-force systemd stop if descendants remain? SendSIGKILL=no; this action will \
                         not silently escalate."
                    ),
                )
            }
        }
        #[cfg(target_os = "linux")]
        PendingAction::RemoveStaleReceipt { display_name, .. } => (
            "Clear dead ownership receipt".into(),
            format!(
                "Remove only the stale machine-local ownership receipt for {display_name:?}? \
                 Portagenty will first prove the exact systemd invocation and exact private \
                 multiplexer target are both absent. This sends no signal and stops no process."
            ),
        ),
        PendingAction::ReplaceStaleBinding {
            session, limits, ..
        } => {
            let memory = limits
                .memory_high_bytes
                .map(compact_bytes)
                .unwrap_or_else(|| "unset".into());
            let memory_max = limits
                .memory_max_bytes
                .map(compact_bytes)
                .unwrap_or_else(|| "unset".into());
            let swap_max = limits
                .memory_swap_max_bytes
                .map(compact_bytes)
                .unwrap_or_else(|| "unset".into());
            let cpu = limits
                .cpu_quota_percent
                .map(|value| format!("{value}%"))
                .unwrap_or_else(|| "unset".into());
            let tasks = limits
                .tasks_max
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unset".into());
            (
                "Replace stale supervised binding".into(),
                format!(
                    "Prove the exact dead receipt for {:?} has no systemd invocation or private multiplexer target, remove only that receipt without sending a signal, then relaunch it supervised with MemoryHigh {memory}, MemoryMax {memory_max}, SwapMax {swap_max}, CPU {cpu}, and TasksMax {tasks}?",
                    session.name
                ),
            )
        }
        PendingAction::SwitchMpx {
            from,
            to,
            live_in_current,
        } => {
            let from_name = match from {
                crate::domain::Multiplexer::Tmux => "tmux",
                crate::domain::Multiplexer::Zellij => "zellij",
                crate::domain::Multiplexer::Wezterm => "wezterm",
            };
            let to_name = match to {
                crate::domain::Multiplexer::Tmux => "tmux",
                crate::domain::Multiplexer::Zellij => "zellij",
                crate::domain::Multiplexer::Wezterm => "wezterm",
            };
            let extra = if *live_in_current >= 2 {
                format!(
                    " {live_in_current} sessions are currently live in {from_name}; \
                     they keep running but won't appear in the new mpx until you \
                     migrate or kill them."
                )
            } else if *live_in_current == 1 {
                format!(
                    " 1 session is currently live in {from_name}; it keeps running \
                     but won't appear in {to_name} until you migrate or kill it."
                )
            } else {
                String::new()
            };
            (
                "Switch multiplexer".into(),
                format!(
                    "Change workspace {workspace_name:?} from {from_name} to {to_name}? \
                     The TOML's `multiplexer` field is updated; comments and sessions \
                     stay intact.{extra} You'll need to press Esc back to the picker \
                     and re-enter the workspace for the new mpx adapter to take over."
                ),
            )
        }
    }
}

/// Human-readable column header above the session list. Matches the
/// layout of `row_list_item` at each width tier. Narrow widths don't
/// use columns (they use stacked cards) so there's no header to show.
fn column_header_line(width: u16) -> String {
    // The visible marker is 1 cell, preceded by " highlight" (2) + space (1);
    // the rest of the header just lines up with the data columns below.
    if width >= 80 {
        format!(
            "   {:<18}  {:<30}  {:<24}  {:<11} {}",
            "SESSION", "PATH", "COMMAND", "STATUS", "LAST"
        )
    } else {
        format!("   {:<18}  {:<24}  {}", "SESSION", "COMMAND", "STATUS")
    }
}

/// Does this row have a kind glyph we'd render? Used to decide
/// whether to reserve space on rows that *don't* have one, so the
/// table stays aligned.
fn kind_glyph_present(kind: Option<crate::domain::SessionKind>) -> bool {
    kind_display(kind).0.is_some()
}

/// Pad the string with spaces to exactly `width` chars, or truncate
/// with a middle ellipsis if it's too long. Width is measured in
/// chars (proxy for cells — good enough for ASCII-mostly session
/// names / paths).
fn pad_or_truncate(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count == width {
        s.to_string()
    } else if count < width {
        format!("{s}{}", " ".repeat(width - count))
    } else if width <= 1 {
        s.chars().take(width).collect()
    } else {
        // Middle ellipsis: keep the start and the end, drop the middle.
        // Paths are more recognizable by their leaf, so bias the split
        // toward preserving the trailing portion.
        let ell = "…";
        let keep = width - 1;
        let tail = (keep * 2).div_ceil(3);
        let head = keep - tail;
        let head_str: String = s.chars().take(head).collect();
        let tail_start = count - tail;
        let tail_str: String = s.chars().skip(tail_start).collect();
        format!("{head_str}{ell}{tail_str}")
    }
}

/// Compact a filesystem path for display:
///   - replace the user's $HOME prefix with `~`
///   - leave the rest alone (truncation happens at padding time)
fn compact_path(p: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            if p == home {
                return "~".to_string();
            }
            let home_slash = format!("{home}/");
            if let Some(rest) = p.strip_prefix(&home_slash) {
                return format!("~/{rest}");
            }
        }
    }
    p.to_string()
}

/// Per-kind display — glyph + style. `None` for Shell/Other since the
/// kind adds no visual clarity there. Colors kept to the standard 8
/// so the output works on plain terminals over SSH.
fn kind_display(kind: Option<crate::domain::SessionKind>) -> (Option<char>, Style) {
    let Some(kind) = kind else {
        return (None, Style::default());
    };
    use crate::domain::SessionKind;
    let color = match kind {
        SessionKind::ClaudeCode => Color::Blue,
        SessionKind::Opencode => Color::Cyan,
        SessionKind::Editor => Color::Magenta,
        SessionKind::DevServer => Color::Green,
        SessionKind::Shell | SessionKind::Other => return (None, Style::default()),
    };
    (
        kind.marker(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Multiplexer as MpxEnum, Session, Workspace};
    use crate::mux::MockMultiplexer;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serial_test::serial;
    use std::path::PathBuf;

    fn sample_workspace(name: &str, sessions: usize) -> Workspace {
        Workspace {
            name: name.into(),
            id: None,
            file_path: None,
            multiplexer: MpxEnum::Tmux,
            projects: vec![],
            sessions: (0..sessions)
                .map(|i| Session {
                    name: format!("s{i}"),
                    cwd: PathBuf::from("/tmp"),
                    command: "true".into(),
                    kind: None,
                    env: std::collections::BTreeMap::new(),
                    description: None,
                })
                .collect(),
            tags: vec![],
        }
    }

    fn render_to_backend(app: &mut App, w: u16, h: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        terminal
    }

    #[test]
    fn renders_header_with_workspace_name_and_session_count() {
        let ws = sample_workspace("Agentic", 3);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let terminal = render_to_backend(&mut app, 60, 10);

        let buffer = terminal.backend().buffer();
        let first_line: String = (0..60)
            .map(|x| buffer[(x, 0)].symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            first_line.contains("Agentic"),
            "header missing name: {first_line:?}"
        );
        assert!(
            first_line.contains("3 sessions"),
            "header missing count: {first_line:?}"
        );
    }

    #[test]
    fn renders_singular_when_one_session() {
        let ws = sample_workspace("Solo", 1);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let terminal = render_to_backend(&mut app, 60, 10);

        let buffer = terminal.backend().buffer();
        let first_line: String = (0..60)
            .map(|x| buffer[(x, 0)].symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(first_line.contains("1 session "), "got: {first_line:?}");
        assert!(!first_line.contains("1 sessions"));
    }

    #[test]
    fn renders_footer_with_back_hint() {
        let ws = sample_workspace("X", 0);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        // Height needs to be big enough for title + col_header + list + 2 footer lines.
        let terminal = render_to_backend(&mut app, 60, 6);

        let buffer = terminal.backend().buffer();
        // "quit" should be in one of the last 2 rows (2-line footer).
        let row4: String = (0..60)
            .map(|x| buffer[(x, 4)].symbol().chars().next().unwrap_or(' '))
            .collect();
        let row5: String = (0..60)
            .map(|x| buffer[(x, 5)].symbol().chars().next().unwrap_or(' '))
            .collect();
        let both = format!("{row4} {row5}");
        // Footer used to say "quit"; after the q-goes-back change it
        // says "back" (Esc/q back to picker). Ctrl+C still hard-quits.
        assert!(both.contains("back"), "got: {both:?}");
    }

    #[test]
    fn handles_narrow_terminal_without_panic() {
        // Termux / small-screen constraint: single-column, tight rows.
        let ws = sample_workspace("narrow", 5);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let _ = render_to_backend(&mut app, 20, 10);
    }

    #[test]
    fn handles_very_short_terminal() {
        // Minimum: header + one row for body + footer = 3 rows.
        let ws = sample_workspace("tiny", 0);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let _ = render_to_backend(&mut app, 80, 3);
    }

    fn workspace_with_description(desc: &str, cmd: &str) -> Workspace {
        Workspace {
            name: "ws".into(),
            id: None,
            file_path: None,
            multiplexer: MpxEnum::Tmux,
            projects: vec![],
            sessions: vec![Session {
                name: "agent".into(),
                cwd: PathBuf::from("/home/u/work/api"),
                command: cmd.into(),
                kind: None,
                env: std::collections::BTreeMap::new(),
                description: Some(desc.into()),
            }],
            tags: vec![],
        }
    }

    fn full_screen(t: &Terminal<TestBackend>, h: u16) -> String {
        (0..h).map(|y| line_at(t, y)).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn selected_row_expands_to_show_description_and_real_command() {
        // The regression the description feature introduced: an
        // annotated row's COMMAND cell shows the note, hiding the real
        // command. Expand-on-select must surface both.
        let ws = workspace_with_description(
            "run the nightly eval sweep and do not kill it",
            "claude --resume --dangerously-skip-permissions",
        );
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let t = render_to_backend(&mut app, 100, 16);
        let screen = full_screen(&t, 16);
        assert!(
            screen.contains("nightly eval sweep"),
            "full description not expanded:\n{screen}"
        );
        assert!(
            screen.contains("cmd"),
            "cmd label missing from expansion:\n{screen}"
        );
        assert!(
            screen.contains("claude --resume"),
            "real command still hidden:\n{screen}"
        );
    }

    #[test]
    fn z_toggles_expansion_off() {
        let ws = workspace_with_description(
            "some long note about what this session is for",
            "claude --resume --dangerously-skip-permissions",
        );
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('z'), KeyModifiers::NONE);
        let t = render_to_backend(&mut app, 100, 16);
        let screen = full_screen(&t, 16);
        // With expansion off, the real command (only visible in the
        // expansion when a description overrides the COMMAND cell) is
        // gone again.
        assert!(
            !screen.contains("--dangerously-skip-permissions"),
            "expansion still rendered after z:\n{screen}"
        );
    }

    #[test]
    fn expansion_survives_short_terminal_without_panic() {
        // Bounded expansion (<=3 desc lines + cmd + cwd) must not
        // panic even when the item is taller than the viewport.
        let ws = workspace_with_description(
            "a very long description that will wrap across several lines when the terminal \
             is wide enough to show it but the terminal here is short so clipping applies",
            "claude",
        );
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let _ = render_to_backend(&mut app, 40, 5);
        let _ = render_to_backend(&mut app, 100, 6);
    }

    fn line_at(t: &Terminal<TestBackend>, y: u16) -> String {
        let buf = t.backend().buffer();
        let w = buf.area().width;
        (0..w)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// Y-coordinate of the first body row (first session row in wide
    /// mode, first card line in narrow). Accounts for the column
    /// header row we add when width >= 60.
    fn first_body_row(width: u16) -> u16 {
        if width >= 60 {
            2
        } else {
            1
        }
    }

    #[test]
    fn renders_each_session_name_in_body() {
        let ws = sample_workspace("multi", 3);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let terminal = render_to_backend(&mut app, 100, 10);

        // Body lives on rows 1..h-1 (row 0 = header, row h-1 = footer).
        let body: String = (1..9)
            .map(|y| line_at(&terminal, y))
            .collect::<Vec<_>>()
            .join("\n");
        for i in 0..3 {
            let expected = format!("s{i}");
            assert!(body.contains(&expected), "missing {expected:?} in:\n{body}");
        }
    }

    #[test]
    fn renders_session_cwd_and_command_alongside_name() {
        let ws = Workspace {
            name: "x".into(),
            id: None,
            file_path: None,
            multiplexer: MpxEnum::Tmux,
            projects: vec![],
            sessions: vec![Session {
                name: "claude".into(),
                cwd: PathBuf::from("/tmp/demo"),
                command: "claude --resume".into(),
                kind: None,
                env: std::collections::BTreeMap::new(),
                description: None,
            }],
            tags: vec![],
        };
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        // This test asserts the collapsed single-line column layout;
        // turn off expand-on-select so row Y-coords are stable.
        app.expand_selected = false;
        let terminal = render_to_backend(&mut app, 100, 5);

        let body = line_at(&terminal, first_body_row(100));
        assert!(body.contains("claude"), "name missing: {body:?}");
        assert!(body.contains("/tmp/demo"), "cwd missing: {body:?}");
        assert!(body.contains("--resume"), "command missing: {body:?}");
    }

    #[test]
    fn empty_workspace_shows_placeholder() {
        let ws = sample_workspace("empty", 0);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let terminal = render_to_backend(&mut app, 60, 5);

        let body = line_at(&terminal, first_body_row(60));
        assert!(
            body.to_lowercase().contains("no sessions"),
            "missing placeholder: {body:?}"
        );
    }

    #[test]
    fn large_session_list_does_not_panic() {
        // 80 sessions into a 20-row terminal — ratatui's List handles
        // overflow by truncating; we just confirm we don't panic.
        let ws = sample_workspace("big", 80);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let _ = render_to_backend(&mut app, 80, 20);
    }

    #[test]
    fn selection_starts_at_zero_for_non_empty() {
        let ws = sample_workspace("x", 3);
        let app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        assert_eq!(app.selected(), Some(0));
    }

    #[test]
    fn selection_is_none_for_empty_workspace() {
        let ws = sample_workspace("x", 0);
        let app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        assert_eq!(app.selected(), None);
    }

    #[test]
    fn j_key_advances_selection_wrapping() {
        let ws = sample_workspace("x", 3);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(1));
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(2));
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(0), "should wrap");
    }

    #[test]
    fn k_key_retreats_selection_wrapping() {
        let ws = sample_workspace("x", 3);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(2), "should wrap to last");
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(1));
    }

    #[test]
    fn arrow_keys_are_equivalent_to_jk() {
        let ws = sample_workspace("x", 4);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(2));
        app.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(1));
    }

    #[test]
    fn g_goes_to_top_capital_g_to_bottom() {
        let ws = sample_workspace("x", 5);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(app.selected(), Some(4));
        app.handle_key(KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(0));
    }

    #[test]
    fn navigation_is_noop_on_empty_workspace() {
        let ws = sample_workspace("x", 0);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(app.selected(), None);
    }

    #[test]
    fn enter_returns_launch_action_with_selected_index() {
        let ws = sample_workspace("x", 3);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        let action = app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(action, Action::LaunchSelected);
        assert_eq!(app.selected(), Some(1));
    }

    #[test]
    fn enter_on_empty_workspace_does_nothing_meaningful() {
        // handle_key returns LaunchSelected, but reduce_action turns it
        // into None because selected() is None.
        let ws = sample_workspace("x", 0);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let action = app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(action, Action::LaunchSelected);
        assert_eq!(app.selected(), None);
    }

    #[test]
    fn quit_keys_return_expected_actions() {
        // q / Ctrl+Q → Back (close session view, return to picker).
        // Ctrl+C → hard Quit (exit pa entirely).
        for (key, expected) in [
            ((KeyCode::Char('q'), KeyModifiers::NONE), Action::Back),
            ((KeyCode::Char('q'), KeyModifiers::CONTROL), Action::Back),
            ((KeyCode::Char('c'), KeyModifiers::CONTROL), Action::Quit),
        ] {
            let ws = sample_workspace("x", 2);
            let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
            let action = app.handle_key(key.0, key.1);
            assert_eq!(action, expected, "key {key:?} should return {expected:?}");
        }
    }

    #[test]
    fn esc_returns_back_action() {
        let ws = sample_workspace("x", 2);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let action = app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(action, Action::Back);
    }

    #[test]
    fn highlight_symbol_appears_next_to_selected_row() {
        let ws = sample_workspace("x", 3);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE); // select index 1
        let terminal = render_to_backend(&mut app, 80, 10);
        // Body starts at first_body_row(80). First session is there,
        // second session (selected) is the next line.
        let first = first_body_row(80);
        let selected = line_at(&terminal, first + 1);
        assert!(
            selected.contains("▶"),
            "expected highlight on selected row, got: {selected:?}"
        );
        let non_selected = line_at(&terminal, first);
        assert!(
            !non_selected.contains("▶"),
            "unexpected highlight on non-selected row: {non_selected:?}"
        );
    }

    // ----------------------------------------------------------------
    // Termux / mobile-SSH rendering contract. See DESIGN.md §10.
    //
    // Typical sizes: 35–45 cols × 15–25 rows in portrait; less with
    // the software keyboard open. These tests anchor the TUI's
    // behavior at those sizes so we don't regress on the mobile path
    // while iterating on layout.
    // ----------------------------------------------------------------

    #[rstest::rstest]
    #[case::phone_portrait(35, 20)]
    #[case::phone_portrait_with_keyboard(40, 15)]
    #[case::phone_portrait_tight(30, 12)]
    #[case::phone_landscape(80, 18)]
    fn renders_cleanly_at_termux_sizes(#[case] w: u16, #[case] h: u16) {
        let ws = sample_workspace("mobile", 4);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let terminal = render_to_backend(&mut app, w, h);

        // Header on row 0 always has the workspace name.
        let header = line_at(&terminal, 0);
        assert!(
            header.contains("mobile"),
            "header missing at {w}x{h}: {header:?}"
        );
        // Footer spans the last 2 rows. `back` (Esc/q) is on one of
        // them — `quit` got renamed to `back` when q became the
        // return-to-picker key.
        let footer1 = line_at(&terminal, h - 2);
        let footer2 = line_at(&terminal, h - 1);
        let both = format!("{footer1} {footer2}");
        assert!(
            both.to_lowercase().contains("back"),
            "footer missing back at {w}x{h}: {both:?}"
        );
        // Selected row (index 0 by default) has the highlight marker
        // somewhere in the body region (rows 1..h-1).
        let has_highlight = (1..h - 1).any(|y| line_at(&terminal, y).contains("▶"));
        assert!(
            has_highlight,
            "no highlight marker visible at {w}x{h}; rendered:\n{}",
            (0..h)
                .map(|y| line_at(&terminal, y))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn termux_on_screen_keyboards_can_navigate_without_modifiers() {
        // Some Android software keyboards send uppercase letters as
        // `Char('G')` with modifiers = NONE rather than SHIFT. Our
        // match arms use `_` for modifiers so either works; this test
        // pins that behavior.
        let ws = sample_workspace("x", 4);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);

        app.handle_key(KeyCode::Char('G'), KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(3), "G without SHIFT should go to last");

        app.handle_key(KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(0), "g should go to first");
    }

    #[test]
    fn termux_volume_down_as_ctrl_quits() {
        // Termux's default mapping of Volume-Down-as-Ctrl arrives as
        // KeyModifiers::CONTROL on a letter key. Ctrl-C must still quit.
        let ws = sample_workspace("x", 2);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let action = app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(action, Action::Quit);
    }

    #[test]
    fn arrow_keys_work_as_fallback_for_jk() {
        // Termux's Extra Keys row provides arrow keys explicitly;
        // some users prefer them to j/k.
        let ws = sample_workspace("x", 4);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        app.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(1));
    }

    #[test]
    fn home_end_work_as_fallback_for_top_bottom() {
        // Same reason — Home/End are easier to reach than g/G on some
        // on-screen keyboards.
        let ws = sample_workspace("x", 4);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::End, KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(3));
        app.handle_key(KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(0));
    }

    // ----------------------------------------------------------------
    // Untracked-session adoption (DESIGN §9). Tests that the TUI
    // surfaces live mpx sessions that weren't part of the loaded
    // workspace, and that Enter maps to the right Multiplexer call
    // based on row state.
    // ----------------------------------------------------------------

    /// Tracked live session: prefixed with "x-" to match what
    /// build_rows computes for workspace "x" + session name.
    fn live_session(name: &str) -> SessionInfo {
        SessionInfo {
            name: format!("x-{name}"),
            cwd: None,
            attached: None,
        }
    }

    /// Untracked live session with bare name.
    fn live_session_bare(name: &str) -> SessionInfo {
        SessionInfo {
            name: name.into(),
            cwd: None,
            attached: None,
        }
    }

    fn drive_enter(app: &mut App) -> Option<AppOutcome> {
        let a = app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        app.reduce_action(a)
    }

    #[test]
    fn untracked_session_appears_in_rows() {
        let ws = sample_workspace("x", 2);
        let app = App::new(
            ws,
            Box::new(MockMultiplexer::new()),
            vec![live_session_bare("stranger")],
        );
        let rows = app.rows();
        assert_eq!(rows.len(), 3, "2 tracked + 1 untracked expected");
        assert_eq!(rows[2].display_name, "stranger");
        assert_eq!(rows[2].state, SessionState::Untracked);
    }

    #[test]
    fn tracked_row_flips_to_live_when_mpx_reports_same_name() {
        // sample_workspace names sessions "s0", "s1", etc. — no
        // sanitization change.
        let ws = sample_workspace("x", 3);
        let app = App::new(
            ws,
            Box::new(MockMultiplexer::new()),
            vec![live_session("s1")],
        );
        let rows = app.rows();
        assert_eq!(rows[0].state, SessionState::NotStarted);
        assert_eq!(rows[1].state, SessionState::Live);
        assert_eq!(rows[2].state, SessionState::NotStarted);
    }

    #[test]
    fn enter_on_not_started_creates_and_attaches() {
        let ws = sample_workspace("x", 2);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let outcome = drive_enter(&mut app).expect("enter should produce outcome");
        match outcome {
            AppOutcome::Launch(LaunchKind::Create { session, mpx_name }) => {
                assert_eq!(session.name, "s0");
                assert!(
                    mpx_name.contains("s0"),
                    "mpx_name should contain session name: {mpx_name}"
                );
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn row_rebuild_preserves_declared_selection_by_name_after_reorder() {
        let ws = sample_workspace("x", 3);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.list_state.select(Some(1));
        assert_eq!(app.selected_row().unwrap().display_name, "s1");

        app.workspace.sessions.swap(0, 1);
        app.rebuild_rows(&[]);

        assert_eq!(app.selected_row().unwrap().display_name, "s1");
    }

    #[test]
    fn row_rebuild_preserves_selection_when_legacy_workspace_gains_uuid() {
        let ws = sample_workspace("x", 2);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.list_state.select(Some(1));
        assert_eq!(app.selected_row().unwrap().display_name, "s1");

        app.workspace.id = Some("550e8400-e29b-41d4-a716-446655440000".into());
        app.rebuild_rows(&[]);

        assert_eq!(app.selected_row().unwrap().display_name, "s1");
        assert!(app.selected_row().unwrap().logical_id.is_some());
    }

    #[test]
    fn enter_on_live_attaches_by_mpx_name() {
        let ws = sample_workspace("x", 1);
        let mut app = App::new(
            ws,
            Box::new(MockMultiplexer::new()),
            vec![live_session("s0")],
        );
        // Row 0 is now Live.
        let outcome = drive_enter(&mut app).expect("enter should produce outcome");
        match outcome {
            AppOutcome::Launch(LaunchKind::Attach { mpx_name, .. }) => {
                assert_eq!(mpx_name, "x-s0");
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_untracked_attaches_by_mpx_name() {
        // Empty workspace, only untracked sessions in mpx.
        let ws = sample_workspace("x", 0);
        let mut app = App::new(
            ws,
            Box::new(MockMultiplexer::new()),
            vec![live_session_bare("orphan-session")],
        );
        let outcome = drive_enter(&mut app).expect("enter should produce outcome");
        match outcome {
            AppOutcome::Launch(LaunchKind::Attach { mpx_name, .. }) => {
                assert_eq!(mpx_name, "orphan-session");
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_empty_everything_produces_no_outcome() {
        let ws = sample_workspace("x", 0);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let outcome = drive_enter(&mut app);
        assert!(
            outcome.is_none(),
            "no rows at all -> no outcome; got {outcome:?}"
        );
    }

    #[test]
    fn rendered_row_shows_state_marker_for_each_state() {
        let ws = sample_workspace("x", 2);
        // s0 live, s1 not-started, plus "extra" untracked.
        let mut app = App::new(
            ws,
            Box::new(MockMultiplexer::new()),
            vec![live_session("s0"), live_session_bare("extra")],
        );
        // Asserts one-line-per-row ordering; disable expand-on-select
        // so the selected row doesn't push the others down.
        app.expand_selected = false;
        let terminal = render_to_backend(&mut app, 100, 10);
        // Body begins at first_body_row. Three rows expected in order:
        // row N   = s0 (live ●), N+1 = s1 (not-started ○),
        // row N+2 = extra (untracked ?).
        let n = first_body_row(100);
        let row1 = line_at(&terminal, n);
        let row2 = line_at(&terminal, n + 1);
        let row3 = line_at(&terminal, n + 2);
        assert!(row1.contains("●"), "row1 should have live marker: {row1:?}");
        assert!(
            row2.contains("○"),
            "row2 should have not-started marker: {row2:?}"
        );
        assert!(
            row3.contains("?"),
            "row3 should have untracked marker: {row3:?}"
        );
        // State and ownership labels also appear.
        let body = format!("{row1}\n{row2}\n{row3}");
        assert!(body.contains("live"));
        assert!(body.contains("idle"));
        assert!(body.contains("untracked"));
    }

    #[test]
    fn header_shows_untracked_count_when_present() {
        let ws = sample_workspace("x", 1);
        let mut app = App::new(
            ws,
            Box::new(MockMultiplexer::new()),
            vec![live_session_bare("other"), live_session_bare("another")],
        );
        let terminal = render_to_backend(&mut app, 80, 5);
        let header = line_at(&terminal, 0);
        assert!(header.contains("2 untracked"), "header missing: {header:?}");
    }

    #[test]
    fn header_omits_untracked_segment_when_zero() {
        let ws = sample_workspace("x", 2);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let terminal = render_to_backend(&mut app, 80, 5);
        let header = line_at(&terminal, 0);
        assert!(
            !header.contains("untracked"),
            "header shouldn't mention untracked when none: {header:?}"
        );
    }

    // ----------------------------------------------------------------
    // kind: hint rendering (ROADMAP v1.x #9).
    // ----------------------------------------------------------------

    fn ws_with_kinds(items: Vec<(&str, Option<crate::domain::SessionKind>)>) -> Workspace {
        Workspace {
            name: "x".into(),
            id: None,
            file_path: None,
            multiplexer: MpxEnum::Tmux,
            projects: vec![],
            sessions: items
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

    #[test]
    fn renders_kind_markers_for_known_kinds() {
        use crate::domain::SessionKind;
        let ws = ws_with_kinds(vec![
            ("claude", Some(SessionKind::ClaudeCode)),
            ("opencode", Some(SessionKind::Opencode)),
            ("editor", Some(SessionKind::Editor)),
            ("dev", Some(SessionKind::DevServer)),
            ("shell", Some(SessionKind::Shell)),
            ("other", Some(SessionKind::Other)),
            ("notype", None),
        ]);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        // One glyph row per session — disable expand-on-select so the
        // selected row's detail lines don't shift the rows below.
        app.expand_selected = false;
        let terminal = render_to_backend(&mut app, 120, 12);

        // Body begins at first_body_row(120); seven rows follow.
        let base = first_body_row(120);
        let row_for = |idx: u16| line_at(&terminal, base + idx);
        assert!(
            row_for(0).contains(" C "),
            "claude row missing C: {:?}",
            row_for(0)
        );
        assert!(
            row_for(1).contains(" O "),
            "opencode row missing O: {:?}",
            row_for(1)
        );
        assert!(
            row_for(2).contains(" E "),
            "editor row missing E: {:?}",
            row_for(2)
        );
        assert!(
            row_for(3).contains(" D "),
            "dev-server row missing D: {:?}",
            row_for(3)
        );
        // Shell/Other/None → no kind marker. Check that the row
        // doesn't stray into another kind's letter.
        for (idx, name) in [(4u16, "shell"), (5, "other"), (6, "notype")] {
            let r = row_for(idx);
            assert!(r.contains(name), "row {idx} missing name {name}: {r:?}");
            // Make sure we're not accidentally emitting stray kind letters
            // — the [idle] label contains no C/O/E/D/J etc in uppercase.
            // Weak check: no " C " / " O " / " E " / " D " segment.
            for m in [" C ", " O ", " E ", " D "] {
                assert!(
                    !r.contains(m),
                    "row {idx} ({name}) unexpectedly has kind marker {m:?}: {r:?}"
                );
            }
        }
    }

    // -----------------------------------------------------------
    // Add-session modal tests.
    // -----------------------------------------------------------

    #[test]
    fn add_session_a_key_opens_modal() {
        let ws = sample_workspace("x", 2);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        assert!(app.adding_session.is_none());
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(app.adding_session.is_some());
        let st = app.adding_session.as_ref().unwrap();
        assert_eq!(st.stage, AddStage::Name);
        assert_eq!(st.name, "");
    }

    #[test]
    fn add_session_typing_builds_name_then_command() {
        let ws = sample_workspace("x", 2);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        for ch in ['d', 'e', 'v'] {
            app.handle_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert_eq!(app.adding_session.as_ref().unwrap().name, "dev");
        // Enter advances to command stage.
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            app.adding_session.as_ref().unwrap().stage,
            AddStage::Command
        );
        for ch in ['b', 'u', 'n'] {
            app.handle_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert_eq!(app.adding_session.as_ref().unwrap().command, "bun");
    }

    #[test]
    fn add_session_esc_cancels() {
        let ws = sample_workspace("x", 2);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.adding_session.is_none());
    }

    #[test]
    fn add_session_tab_advances_stage_when_name_nonempty() {
        let ws = sample_workspace("x", 2);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(
            app.adding_session.as_ref().unwrap().stage,
            AddStage::Command
        );
    }

    #[test]
    fn add_session_enter_on_empty_name_shows_error() {
        let ws = sample_workspace("x", 2);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        let st = app.adding_session.as_ref().unwrap();
        assert_eq!(st.stage, AddStage::Name);
        assert!(st.error.is_some());
    }

    #[test]
    fn add_session_ctrl_h_deletes_char() {
        let ws = sample_workspace("x", 2);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        for ch in ['a', 'b', 'c'] {
            app.handle_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Char('h'), KeyModifiers::CONTROL);
        assert_eq!(app.adding_session.as_ref().unwrap().name, "ab");
    }

    #[test]
    fn add_session_commits_to_disk_and_reloads() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let ws_path = tmp.path().join("test.portagenty.toml");
        std::fs::write(
            &ws_path,
            "name = \"test\"\nmultiplexer = \"tmux\"\n\n\
             [[session]]\nname = \"existing\"\ncwd = \".\"\ncommand = \"bash\"\n",
        )
        .unwrap();
        let ws = crate::config::load(&crate::config::LoadOptions {
            workspace_path: Some(ws_path.clone()),
            ..Default::default()
        })
        .unwrap();
        // Reload hits mux.list_sessions(); set expectation to return
        // empty so the reload doesn't panic.
        let mut mock = MockMultiplexer::new();
        mock.expect_list_sessions().returning(|| Ok(vec![]));
        let mut app = App::new(ws, Box::new(mock), vec![]);

        // Open modal, type "newsess", Enter, type "echo hi", Enter.
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        for ch in "newsess".chars() {
            app.handle_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        for ch in "echo hi".chars() {
            app.handle_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        // Modal should be closed and the file should contain the new
        // session.
        assert!(app.adding_session.is_none());
        let raw = std::fs::read_to_string(&ws_path).unwrap();
        assert!(raw.contains("\"newsess\""), "written file:\n{raw}");
        assert!(raw.contains("\"echo hi\""), "written file:\n{raw}");
        // Workspace should have been reloaded — rows now include "newsess".
        assert!(
            app.rows.iter().any(|r| r.display_name == "newsess"),
            "newsess not in rows after add"
        );
    }

    #[test]
    fn o_key_returns_open_shell_with_workspace_dir() {
        let ws = Workspace {
            name: "x".into(),
            id: None,
            file_path: Some(PathBuf::from("/home/u/code/proj/x.portagenty.toml")),
            multiplexer: MpxEnum::Tmux,
            projects: vec![],
            sessions: vec![],
            tags: vec![],
        };
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let action = app.handle_key(KeyCode::Char('o'), KeyModifiers::NONE);
        assert_eq!(
            action,
            Action::OpenShellAt(PathBuf::from("/home/u/code/proj"))
        );
    }

    #[test]
    fn open_shell_action_reduces_to_app_outcome() {
        let ws = sample_workspace("x", 2);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let outcome = app.reduce_action(Action::OpenShellAt(PathBuf::from("/tmp/here")));
        match outcome {
            Some(AppOutcome::OpenShellAt(dir)) => {
                assert_eq!(dir, PathBuf::from("/tmp/here"));
            }
            other => panic!("expected OpenShellAt outcome, got {other:?}"),
        }
    }

    #[test]
    fn t_key_opens_file_tree_browser() {
        let ws = Workspace {
            name: "x".into(),
            id: None,
            file_path: Some(PathBuf::from("/home/u/code/proj/x.portagenty.toml")),
            multiplexer: MpxEnum::Tmux,
            projects: vec![],
            sessions: vec![],
            tags: vec![],
        };
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        assert!(app.browsing.is_none());
        app.handle_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(app.browsing.is_some(), "t should open the file tree");
        // Verify it's in tree mode and rooted at the workspace dir.
        let search = app.browsing.as_ref().unwrap();
        match &search.mode {
            crate::tui::find::FindMode::Tree(tree) => {
                assert_eq!(tree.root, PathBuf::from("/home/u/code/proj"));
            }
            _ => panic!("expected tree mode after pressing t"),
        }
    }

    #[test]
    fn t_then_esc_closes_file_tree() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let ws_path = tmp.path().join("x.portagenty.toml");
        std::fs::write(&ws_path, "name = \"x\"\nmultiplexer = \"tmux\"\n").unwrap();
        let ws = Workspace {
            name: "x".into(),
            id: None,
            file_path: Some(ws_path),
            multiplexer: MpxEnum::Tmux,
            projects: vec![],
            sessions: vec![],
            tags: vec![],
        };
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(app.browsing.is_some());
        // Esc in tree mode → BackToSearch (switch to search submode,
        // not close). So one Esc won't close; Ctrl+C will.
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.browsing.is_none(), "Ctrl+C should cancel the browse");
    }

    #[test]
    fn add_session_empty_command_defaults_to_bash_shell() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let ws_path = tmp.path().join("t.portagenty.toml");
        std::fs::write(
            &ws_path,
            "name = \"t\"\nmultiplexer = \"tmux\"\n\n\
             [[session]]\nname = \"existing\"\ncwd = \".\"\ncommand = \"bash\"\n",
        )
        .unwrap();
        let ws = crate::config::load(&crate::config::LoadOptions {
            workspace_path: Some(ws_path.clone()),
            ..Default::default()
        })
        .unwrap();
        let mut mock = MockMultiplexer::new();
        mock.expect_list_sessions().returning(|| Ok(vec![]));
        let mut app = App::new(ws, Box::new(mock), vec![]);

        // Open modal, type a name, Enter to advance, hit Enter AGAIN
        // on empty command → should default to bash shell.
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        for ch in "plain-shell".chars() {
            app.handle_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        // Empty command — just press Enter.
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        // Modal closed (success).
        assert!(
            app.adding_session.is_none(),
            "empty command should succeed with a default, not error"
        );
        let raw = std::fs::read_to_string(&ws_path).unwrap();
        assert!(raw.contains("\"plain-shell\""), "name missing:\n{raw}");
        assert!(raw.contains("\"bash\""), "command missing:\n{raw}");
        assert!(raw.contains("\"shell\""), "kind missing:\n{raw}");
    }

    #[test]
    fn add_session_rejects_duplicate_name() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let ws_path = tmp.path().join("t.portagenty.toml");
        std::fs::write(
            &ws_path,
            "name = \"t\"\nmultiplexer = \"tmux\"\n\n\
             [[session]]\nname = \"shell\"\ncwd = \".\"\ncommand = \"bash\"\n",
        )
        .unwrap();
        let ws = crate::config::load(&crate::config::LoadOptions {
            workspace_path: Some(ws_path.clone()),
            ..Default::default()
        })
        .unwrap();
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);

        // Try to add a session named "shell" (already exists).
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        for ch in "shell".chars() {
            app.handle_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        for ch in "bash".chars() {
            app.handle_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        // Modal should still be open (commit failed), error present.
        let st = app.adding_session.as_ref().expect("modal should stay open");
        assert!(st.error.is_some(), "expected error on duplicate");
    }

    fn supervised_workspace() -> Workspace {
        let mut workspace = sample_workspace("x", 1);
        workspace.id = Some("550e8400-e29b-41d4-a716-446655440000".into());
        workspace.sessions[0].kind = Some(crate::domain::SessionKind::ClaudeCode);
        workspace
    }

    fn supervised_app() -> App {
        let mut mock = MockMultiplexer::new();
        mock.expect_has_session().returning(|_| Ok(false));
        App::new(supervised_workspace(), Box::new(mock), vec![])
    }

    fn write_supervision_workspace(path: &std::path::Path, id: Option<&str>) {
        let id_line = id.map(|id| format!("id = \"{id}\"\n")).unwrap_or_default();
        std::fs::write(
            path,
            format!(
                "# preserved workspace comment\nname = \"legacy\"\n{id_line}multiplexer = \"tmux\"\n\n[[session]]\nname = \"shell\"\ncwd = \".\"\ncommand = \"bash\"\n"
            ),
        )
        .unwrap();
    }

    #[test]
    #[serial]
    fn idle_legacy_workspace_can_confirm_add_id_and_open_limits() {
        let xdg = assert_fs::TempDir::new().unwrap();
        let home = assert_fs::TempDir::new().unwrap();
        let _env = crate::test_env::EnvSandbox::new()
            .set("XDG_CONFIG_HOME", xdg.path())
            .set("HOME", home.path());
        let temp = assert_fs::TempDir::new().unwrap();
        let path = temp.path().join("legacy.portagenty.toml");
        write_supervision_workspace(&path, None);
        let workspace = crate::config::load(&crate::config::LoadOptions {
            workspace_path: Some(path.clone()),
            ..Default::default()
        })
        .unwrap();
        let mut mock = MockMultiplexer::new();
        mock.expect_list_sessions()
            .times(1)
            .returning(|| Ok(vec![]));
        mock.expect_has_session()
            .withf(|name| name == "legacy-shell")
            .times(1)
            .returning(|_| Ok(false));
        let mut app = App::new(workspace, Box::new(mock), vec![]);
        assert_eq!(app.rows[0].ownership, RowOwnership::NeedsWorkspaceId);

        app.handle_key(KeyCode::Char('S'), KeyModifiers::NONE);
        assert!(matches!(
            app.pending,
            Some(PendingAction::PrepareSupervised {
                assign_workspace_id: true,
                restart_live: false,
                ..
            })
        ));
        let (_, copy) = confirm_copy(app.pending.as_ref().unwrap(), "legacy");
        assert!(copy.contains("Add a stable UUID"), "copy: {copy}");

        app.handle_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(app.supervising.is_some());
        assert_eq!(app.rows[0].ownership, RowOwnership::IdleSupported);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# preserved workspace comment"));
        let parsed: crate::config::WorkspaceFile = crate::config::load_toml(&path).unwrap();
        let assigned_id = parsed.id.unwrap();
        uuid::Uuid::parse_str(&assigned_id).unwrap();

        let run_result = app.finish(AppOutcome::Back);
        assert_eq!(
            run_result.workspace.id.as_deref(),
            Some(assigned_id.as_str()),
            "the outer coordinator must receive the workspace reloaded after UUID assignment"
        );
    }

    #[test]
    #[serial]
    fn cancelling_legacy_supervision_does_not_write_or_stop() {
        let xdg = assert_fs::TempDir::new().unwrap();
        let home = assert_fs::TempDir::new().unwrap();
        let _env = crate::test_env::EnvSandbox::new()
            .set("XDG_CONFIG_HOME", xdg.path())
            .set("HOME", home.path());
        let temp = assert_fs::TempDir::new().unwrap();
        let path = temp.path().join("legacy.portagenty.toml");
        write_supervision_workspace(&path, None);
        let before = std::fs::read_to_string(&path).unwrap();
        let workspace = crate::config::load(&crate::config::LoadOptions {
            workspace_path: Some(path.clone()),
            ..Default::default()
        })
        .unwrap();
        let mut app = App::new(workspace, Box::new(MockMultiplexer::new()), vec![]);

        app.handle_key(KeyCode::Char('S'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);

        assert!(app.pending.is_none());
        assert!(app.supervising.is_none());
        assert_eq!(std::fs::read_to_string(path).unwrap(), before);
    }

    #[test]
    #[serial]
    fn live_legacy_workspace_persists_id_before_exact_target_restart() {
        let xdg = assert_fs::TempDir::new().unwrap();
        let home = assert_fs::TempDir::new().unwrap();
        let _env = crate::test_env::EnvSandbox::new()
            .set("XDG_CONFIG_HOME", xdg.path())
            .set("HOME", home.path());
        let temp = assert_fs::TempDir::new().unwrap();
        let path = temp.path().join("legacy.portagenty.toml");
        write_supervision_workspace(&path, None);
        let workspace = crate::config::load(&crate::config::LoadOptions {
            workspace_path: Some(path.clone()),
            ..Default::default()
        })
        .unwrap();
        let live = vec![SessionInfo {
            name: "legacy-shell".into(),
            cwd: None,
            attached: Some(2),
        }];
        let mut sequence = mockall::Sequence::new();
        let mut mock = MockMultiplexer::new();
        let refreshed_live = live.clone();
        mock.expect_list_sessions()
            .times(1)
            .in_sequence(&mut sequence)
            .return_once(move || Ok(refreshed_live));
        let path_at_kill = path.clone();
        mock.expect_kill()
            .withf(|name| name == "legacy-shell")
            .times(1)
            .in_sequence(&mut sequence)
            .returning(move |_| {
                let raw = std::fs::read_to_string(&path_at_kill).unwrap();
                assert!(raw.contains("id = "), "ID must exist before kill: {raw}");
                Ok(())
            });
        mock.expect_list_sessions()
            .times(1)
            .in_sequence(&mut sequence)
            .return_once(|| Ok(vec![]));
        mock.expect_has_session()
            .withf(|name| name == "legacy-shell")
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|_| Ok(false));
        let mut app = App::new(workspace, Box::new(mock), live);

        app.handle_key(KeyCode::Char('S'), KeyModifiers::NONE);
        let (_, copy) = confirm_copy(app.pending.as_ref().unwrap(), "legacy");
        assert!(copy.contains("2 clients"), "copy: {copy}");
        assert!(copy.contains("not migrated or claimed"), "copy: {copy}");
        app.handle_key(KeyCode::Char('y'), KeyModifiers::NONE);

        assert!(app.supervising.is_some());
        assert_eq!(app.rows[0].state, SessionState::NotStarted);
        assert_eq!(app.rows[0].ownership, RowOwnership::IdleSupported);
    }

    #[test]
    fn live_uuid_row_offers_restart_but_owned_and_invalid_rows_refuse() {
        let live = vec![SessionInfo {
            name: "x-s0".into(),
            cwd: None,
            attached: Some(1),
        }];
        let mut app = App::new(
            supervised_workspace(),
            Box::new(MockMultiplexer::new()),
            live,
        );
        app.handle_key(KeyCode::Char('S'), KeyModifiers::NONE);
        assert!(matches!(
            app.pending,
            Some(PendingAction::PrepareSupervised {
                assign_workspace_id: false,
                restart_live: true,
                ..
            })
        ));

        app.pending = None;
        app.rows[0].ownership = RowOwnership::Owned;
        app.handle_key(KeyCode::Char('S'), KeyModifiers::NONE);
        assert!(app.pending.is_none());
        assert!(app
            .status
            .as_deref()
            .is_some_and(|text| text.contains("already supervised")));

        app.rows[0].ownership = RowOwnership::InvalidWorkspaceId;
        app.handle_key(KeyCode::Char('S'), KeyModifiers::NONE);
        assert!(app.pending.is_none());
        assert!(app
            .status
            .as_deref()
            .is_some_and(|text| text.contains("invalid ID")));
    }

    #[test]
    fn target_reappearance_keeps_limits_modal_open_and_refuses_launch() {
        let mut mock = MockMultiplexer::new();
        mock.expect_has_session()
            .withf(|name| name == "x-s0")
            .times(1)
            .returning(|_| Ok(true));
        let mut app = App::new(supervised_workspace(), Box::new(mock), vec![]);
        app.handle_key(KeyCode::Char('S'), KeyModifiers::NONE);
        for _ in 0..4 {
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        }
        let action = app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(action, Action::None);
        let state = app.supervising.as_ref().expect("modal should remain open");
        assert!(state
            .error
            .as_deref()
            .is_some_and(|text| text.contains("became live")));
    }

    #[test]
    fn capability_failure_prevents_confirmation_or_limits_modal() {
        fn unavailable() -> Result<()> {
            anyhow::bail!("test backend unavailable")
        }

        let mut app = App::new(
            supervised_workspace(),
            Box::new(MockMultiplexer::new()),
            vec![],
        );
        app.supervision_preflight = unavailable;
        app.handle_key(KeyCode::Char('S'), KeyModifiers::NONE);
        assert!(app.pending.is_none());
        assert!(app.supervising.is_none());
        assert!(app
            .status
            .as_deref()
            .is_some_and(|text| text.contains("backend unavailable")));
    }

    #[cfg(target_os = "linux")]
    fn test_receipt() -> BindingReceipt {
        BindingReceipt {
            schema_version: crate::supervision::model::LEGACY_RECEIPT_SCHEMA_VERSION,
            logical_id: LogicalSessionId::new("550e8400-e29b-41d4-a716-446655440000", "s0")
                .unwrap(),
            backend: crate::supervision::BackendKind::SystemdUserService,
            unit_name: "portagenty-wtest.service".into(),
            invocation_id: "00112233445566778899aabbccddeeff".into(),
            control_group: "/user.slice/user-1000.slice/user@1000.service/app.slice/test.scope"
                .into(),
            mux_target: crate::supervision::MuxTarget::TmuxPrivate {
                socket: PathBuf::from("/run/user/1000/portagenty/test/tmux.sock"),
                session: "opaque-owned-session".into(),
            },
            observed_at_unix_ms: 1,
            limits: ResourceLimits::default(),
            session_kind: None,
            requested_slice: None,
            workload_anchor: None,
        }
    }

    #[cfg(target_os = "linux")]
    fn app_with_test_receipt() -> App {
        let mut app = supervised_app();
        let receipt = test_receipt();
        app.receipts
            .insert(receipt.logical_id.clone(), receipt.clone());
        app.apply_receipt_annotations(&BTreeMap::from([(
            receipt.logical_id,
            (RowOwnership::Owned, None, Vec::new()),
        )]));
        app
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unreadable_supervision_evidence_blocks_idle_enter() {
        let mut app = supervised_app();
        app.fail_closed_supervision("evidence unavailable");
        assert!(app.reduce_action(Action::LaunchSelected).is_none());
        assert!(app
            .status
            .as_deref()
            .is_some_and(|status| status.contains("evidence could not be loaded")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pending_launch_row_blocks_enter_supervise_stop_and_force_kill() {
        let logical_id =
            LogicalSessionId::new("550e8400-e29b-41d4-a716-446655440000", "s0").unwrap();
        let pending = crate::supervision::PendingLaunch {
            logical_id,
            unit_name: "portagenty-wpending.service".into(),
            mux_target: crate::supervision::MuxTarget::TmuxPrivate {
                socket: PathBuf::from("/run/user/1000/portagenty/tmux/pending.sock"),
                session: "main".into(),
            },
            marker_path: PathBuf::from(
                "/run/user/1000/portagenty/workloads/0123456789abcdef0123456789abcdef.marker.toml",
            ),
            created_at_unix_ms: 1,
            creator_pid: 123,
            creator_start_time_ticks: 456,
            last_error: Some("interrupted".into()),
        };
        let mut app = supervised_app().with_supervision_evidence(Vec::new(), vec![pending]);
        assert_eq!(app.rows[0].ownership, RowOwnership::Pending);

        assert!(app.reduce_action(Action::LaunchSelected).is_none());
        assert!(app
            .status
            .as_deref()
            .is_some_and(|status| status.contains("pending supervision evidence")));
        for key in ['S', 'x', 'X'] {
            app.pending = None;
            app.handle_key(KeyCode::Char(key), KeyModifiers::NONE);
            assert!(app.pending.is_none());
            assert!(app.supervising.is_none());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn owned_row_attaches_to_exact_receipted_target() {
        let mut app = app_with_test_receipt();
        let expected = test_receipt().mux_target;
        assert_eq!(app.rows[0].ownership, RowOwnership::Owned);
        assert_eq!(app.rows[0].mpx_name, "opaque-owned-session");

        let outcome = app.reduce_action(Action::LaunchSelected);
        match outcome {
            Some(AppOutcome::Launch(LaunchKind::AttachOwned { target, .. })) => {
                assert_eq!(target, expected);
            }
            other => panic!("expected exact owned attach, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_receipt_is_exact_target_attach_only() {
        let mut app = supervised_app();
        let receipt = test_receipt();
        app.receipts
            .insert(receipt.logical_id.clone(), receipt.clone());
        app.apply_receipt_annotations(&BTreeMap::from([(
            receipt.logical_id.clone(),
            (RowOwnership::LegacyRestartRequired, None, Vec::new()),
        )]));

        assert!(matches!(
            app.reduce_action(Action::LaunchSelected),
            Some(AppOutcome::Launch(LaunchKind::AttachOwned { target, .. }))
                if target == receipt.mux_target
        ));
        app.open_kill_prompt();
        assert!(app.pending.is_none());
        assert!(app
            .status
            .as_deref()
            .is_some_and(|text| text.contains("attach-only")));
    }

    #[test]
    fn idle_uuid_row_enter_uses_recommended_supervision_without_modal() {
        let mut app = supervised_app();
        match app.reduce_action(Action::LaunchSelected) {
            Some(AppOutcome::Launch(LaunchKind::CreateSupervised {
                session,
                limits,
                intent,
            })) => {
                assert_eq!(session.name, "s0");
                assert_eq!(limits, ResourceLimits::claude_defaults());
                assert_eq!(intent, SupervisionIntent::RoutineEnter);
                assert!(app.supervising.is_none());
            }
            other => panic!("expected routine supervised launch, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn idle_stale_enter_confirms_exact_replacement_with_recommended_limits() {
        let mut app = app_with_test_receipt();
        app.rows[0].ownership = RowOwnership::Stale;
        app.rebuild_rows(&[]);

        assert!(app.reduce_action(Action::LaunchSelected).is_none());
        assert!(matches!(
            app.pending,
            Some(PendingAction::ReplaceStaleBinding { .. })
        ));
        let (_, copy) = confirm_copy(app.pending.as_ref().unwrap(), "x");
        assert!(copy.contains("without sending a signal"), "copy: {copy}");
        assert!(copy.contains("MemoryHigh 3.0G"), "copy: {copy}");
        assert!(copy.contains("MemoryMax 5.0G"), "copy: {copy}");
        assert!(copy.contains("SwapMax 512M"), "copy: {copy}");

        let action = app.handle_key(KeyCode::Char('y'), KeyModifiers::NONE);
        match action {
            Action::LaunchStaleSupervised {
                session,
                receipt,
                limits,
            } => {
                assert_eq!(session.name, "s0");
                assert_eq!(*receipt, test_receipt());
                assert_eq!(limits, ResourceLimits::claude_defaults());
            }
            other => panic!("expected stale supervised action, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn row_rebuild_preserves_receipt_ownership_and_resource_text() {
        let mut app = app_with_test_receipt();
        app.rows[0].ownership = RowOwnership::Stale;
        app.rows[0].resource_summary = Some("CPU 80% · memory 2.0 GiB".into());
        app.rows[0].resource_details = vec!["memory high=3".into()];

        app.rebuild_rows(&[]);

        assert_eq!(app.rows[0].ownership, RowOwnership::Stale);
        assert_eq!(app.rows[0].state, SessionState::NotStarted);
        assert_eq!(app.rows[0].mpx_name, "x-s0");
        assert_eq!(
            app.rows[0].resource_summary.as_deref(),
            Some("CPU 80% · memory 2.0 GiB")
        );
        assert_eq!(app.rows[0].resource_details, vec!["memory high=3"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unreconciled_receipt_does_not_impersonate_a_live_target() {
        let mut app = supervised_app();
        let receipt = test_receipt();
        app.receipts
            .insert(receipt.logical_id.clone(), receipt.clone());
        app.apply_receipt_annotations(&BTreeMap::new());

        assert_eq!(app.rows[0].ownership, RowOwnership::ExistingUnverified);
        assert_eq!(app.rows[0].state, SessionState::NotStarted);
        assert_eq!(app.rows[0].mpx_name, "x-s0");
        assert!(app.reduce_action(Action::LaunchSelected).is_none());
        assert!(app
            .status
            .as_deref()
            .is_some_and(|text| text.contains("verifying")));

        app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(app.pending.is_none());
        assert!(app
            .status
            .as_deref()
            .is_some_and(|text| text.contains("verifying")));

        app.handle_key(KeyCode::Char('S'), KeyModifiers::NONE);
        assert!(app.pending.is_none());
        assert!(app.supervising.is_none());
        assert!(app
            .status
            .as_deref()
            .is_some_and(|text| text.contains("verifying")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_receipt_attaches_the_real_ordinary_live_target() {
        let mut app = App::new(
            supervised_workspace(),
            Box::new(MockMultiplexer::new()),
            vec![live_session("s0")],
        );
        let receipt = test_receipt();
        app.receipts
            .insert(receipt.logical_id.clone(), receipt.clone());
        app.apply_receipt_annotations(&BTreeMap::from([(
            receipt.logical_id,
            (RowOwnership::Stale, None, Vec::new()),
        )]));

        assert_eq!(app.rows[0].ownership, RowOwnership::Stale);
        assert_eq!(app.rows[0].state, SessionState::Live);
        assert_eq!(app.rows[0].mpx_name, "x-s0");
        match app.reduce_action(Action::LaunchSelected) {
            Some(AppOutcome::Launch(LaunchKind::Attach {
                mpx_name,
                display_name,
            })) => {
                assert_eq!(mpx_name, "x-s0");
                assert_eq!(display_name, "s0");
            }
            other => panic!("expected ordinary attach, got {other:?}"),
        }
    }

    #[test]
    fn supervised_launch_modal_prefills_claude_policy() {
        let mut app = supervised_app();
        assert_eq!(
            app.handle_key(KeyCode::Char('S'), KeyModifiers::NONE),
            Action::None
        );
        let state = app.supervising.as_ref().unwrap();
        assert_eq!(state.memory_high, "3G");
        assert_eq!(state.memory_max, "5G");
        assert_eq!(state.memory_swap_max, "512MiB");
        assert_eq!(state.cpu_quota, "800");
        assert_eq!(state.tasks_max, "1200");
        let terminal = render_to_backend(&mut app, 90, 30);
        let screen = full_screen(&terminal, 30);
        for expected in ["3G", "5G", "512MiB", "800", "1200"] {
            assert!(screen.contains(expected), "missing {expected}:\n{screen}");
        }

        for _ in 0..4 {
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        }
        let action = app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            action,
            Action::LaunchSupervisedSelected(ResourceLimits::claude_defaults())
        );
    }

    #[test]
    fn cleared_claude_fields_resolve_back_to_standard_defaults() {
        let mut app = supervised_app();
        app.handle_key(KeyCode::Char('S'), KeyModifiers::NONE);
        for index in 0..5 {
            app.handle_key(KeyCode::Char('u'), KeyModifiers::CONTROL);
            if index < 4 {
                app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
            }
        }
        let action = app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            action,
            Action::LaunchSupervisedSelected(ResourceLimits::claude_defaults())
        );
    }

    #[test]
    fn supervised_launch_modal_parses_all_hard_and_soft_limits() {
        let mut app = supervised_app();
        assert_eq!(
            app.handle_key(KeyCode::Char('S'), KeyModifiers::NONE),
            Action::None
        );
        for value in ["1G", "4G", "256MiB", "250", "42"] {
            app.handle_key(KeyCode::Char('u'), KeyModifiers::CONTROL);
            for ch in value.chars() {
                app.handle_key(KeyCode::Char(ch), KeyModifiers::NONE);
            }
            if value != "42" {
                app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
            }
        }
        let action = app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            action,
            Action::LaunchSupervisedSelected(ResourceLimits {
                memory_high_bytes: Some(1_073_741_824),
                memory_max_bytes: Some(4_294_967_296),
                memory_swap_max_bytes: Some(268_435_456),
                cpu_quota_percent: Some(250.0),
                tasks_max: Some(42),
            })
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_owned_row_offers_receipt_cleanup_but_refuses_force_kill() {
        let mut app = app_with_test_receipt();
        app.rows[0].ownership = RowOwnership::Stale;

        app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(matches!(
            app.pending,
            Some(PendingAction::RemoveStaleReceipt { .. })
        ));
        let (_, copy) = confirm_copy(app.pending.as_ref().unwrap(), "x");
        assert!(copy.contains("sends no signal"), "copy: {copy}");
        assert!(copy.contains("stops no process"), "copy: {copy}");

        app.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('X'), KeyModifiers::NONE);
        assert!(app.pending.is_none());
        assert!(app
            .status
            .as_deref()
            .is_some_and(|text| text.contains("owned-and-verified")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn force_kill_requires_a_separate_owned_confirmation() {
        let mut app = app_with_test_receipt();
        app.handle_key(KeyCode::Char('X'), KeyModifiers::NONE);
        assert!(matches!(
            app.pending,
            Some(PendingAction::StopOwned { force: true, .. })
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn expanded_owned_row_renders_resource_summary_and_details() {
        let mut app = app_with_test_receipt();
        app.rows[0].resource_summary = Some("CPU 80% · memory 2.0 GiB".into());
        app.rows[0].resource_details = vec!["memory events high=3 oom=0".into()];

        let terminal = render_to_backend(&mut app, 100, 14);
        let screen = full_screen(&terminal, 14);
        assert!(
            screen.contains("CPU 80%"),
            "resource summary missing:\n{screen}"
        );
        assert!(
            screen.contains("memory events high=3"),
            "details missing:\n{screen}"
        );
    }
}
