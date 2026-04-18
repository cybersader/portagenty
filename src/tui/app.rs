//! TUI app state + render loop. Ratatui 0.29 + crossterm 0.28.
//!
//! v1 renders a single-column session list over the resolved
//! `domain::Workspace`. Two-pane project/session layouts and the
//! Tags / Custom Groups views come in v1.x per `ROADMAP.md`.

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    prelude::*,
    widgets::{List, ListItem, ListState, Paragraph},
    DefaultTerminal,
};

use crate::domain::{Session, Workspace};
use crate::mux::{Multiplexer, SessionInfo};
use crate::tui::view::{build_rows, SessionRow, SessionState};

/// How the user wants a selected row to be realized on the mpx side.
/// Determined by the row's [`SessionState`].
#[derive(Debug, Clone)]
pub enum LaunchKind {
    /// Workspace-defined, not currently live: `create_and_attach`.
    /// `mpx_name` is the workspace-scoped name the mpx should use.
    Create { session: Session, mpx_name: String },
    /// Already live (workspace or untracked): `attach` by sanitized name.
    Attach { mpx_name: String },
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

/// Internal action dispatch. Returned from [`App::handle_key`] so the
/// event loop can translate a key press into either continued
/// in-TUI work or a reason to exit the loop.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    /// Esc pressed — ask the outer driver to back out to the picker
    /// (or quit if the picker wasn't in the chain).
    Back,
    LaunchSelected,
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
        let rows = build_rows(&workspace, &live);
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
        }
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
    /// or picks a session to launch. Returns the outcome and hands
    /// back the multiplexer so the outer entry point can call
    /// [`Multiplexer::create_and_attach`] after restoring the terminal.
    pub fn run(
        mut self,
        terminal: &mut DefaultTerminal,
    ) -> Result<(AppOutcome, Box<dyn Multiplexer>)> {
        loop {
            // Auto-age the status line so a "cancelled" or "deleted X"
            // message doesn't sit forever when the user walks away.
            if let Some(set_at) = self.status_set_at {
                if set_at.elapsed() >= STATUS_TTL {
                    self.clear_status();
                }
            }
            terminal.draw(|frame| self.render(frame))?;

            // Poll instead of read so we can re-check the status TTL
            // periodically. 250ms is short enough to feel responsive
            // when the message clears, long enough that we're not
            // burning CPU.
            if event::poll(std::time::Duration::from_millis(250))? {
                if let Some(outcome) = self.handle_event()? {
                    return Ok((outcome, self.mux));
                }
            }
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
            Action::LaunchSelected => self.selected().and_then(|i| {
                let row = self.rows.get(i)?;
                let kind = match row.state {
                    SessionState::NotStarted => {
                        row.session.as_ref().map(|s| LaunchKind::Create {
                            session: s.clone(),
                            mpx_name: row.mpx_name.clone(),
                        })?
                    }
                    SessionState::Live | SessionState::Untracked => LaunchKind::Attach {
                        mpx_name: row.mpx_name.clone(),
                    },
                };
                Some(AppOutcome::Launch(kind))
            }),
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
        self.browsing = Some(crate::tui::find::SearchState::tree_at(
            self.workspace_dir(),
        ));
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
        if row.state == SessionState::NotStarted {
            self.set_status("x: no live session to kill on this row (it's idle)");
            return;
        }
        self.pending = Some(PendingAction::KillSession {
            display_name: row.display_name.clone(),
            mpx_name: row.mpx_name.clone(),
            attached_clients: row.attached_clients,
        });
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
                self.rows = crate::tui::view::build_rows(&self.workspace, &live);
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
                    self.rows = crate::tui::view::build_rows(&self.workspace, &live);
                    if self.rows.is_empty() {
                        self.list_state.select(None);
                    } else {
                        let sel = self.list_state.selected().unwrap_or(0);
                        self.list_state.select(Some(sel.min(self.rows.len() - 1)));
                    }
                    self.set_status(format!("killed session {display_name:?}"));
                }
                Err(e) => {
                    self.set_status(format!("kill failed: {e:#}"));
                }
            },
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
                        self.rows = crate::tui::view::build_rows(&self.workspace, &live);
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
                        self.rows = crate::tui::view::build_rows(&self.workspace, &live);
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
                                        self.rows =
                                            crate::tui::view::build_rows(&self.workspace, &live);
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
                crate::tui::confirm::ConfirmKey::Confirm => {
                    self.perform_pending(action);
                }
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
                    Entry::new("Enter/l", "launch"),
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
                    Span::styled("kill  ", Style::default().add_modifier(Modifier::DIM)),
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

        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|r| row_list_item(r, name_col, width, cwd_col, cmd_col, kind_space > 0))
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

fn row_list_item(
    row: &SessionRow,
    name_col: usize,
    width: u16,
    cwd_col: usize,
    cmd_col: usize,
    reserve_kind_space: bool,
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
    let status_label = match row.state {
        SessionState::Live => {
            if let Some(n) = row.attached_clients {
                if n > 1 {
                    format!("[live · {n} clients]")
                } else if n == 1 {
                    "[live · 1 client]".to_string()
                } else {
                    "[live · detached]".to_string()
                }
            } else {
                format!("[{}]", row.state.label())
            }
        }
        _ => format!("[{}]", row.state.label()),
    };

    // Narrow: render each row as a two-line "card". Line 1 is the
    // essentials (marker + name + status tag). Line 2 is a dim,
    // indented detail line showing command and/or cwd. User never has
    // to guess what a cramped column means because the detail line
    // calls each piece out explicitly.
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
        // Detail line: indent under the name, show "cmd · path" with
        // tolerable middle-truncation so it always fits the width.
        let cmd = row.command_display.clone();
        let path = compact_path(&row.cwd_display);
        let detail_budget = (width as usize).saturating_sub(6).max(10);
        let raw_detail = if cmd == "(unknown)" {
            path
        } else if path == "(unknown)" || path.is_empty() {
            cmd
        } else {
            format!("{cmd}  ·  {path}")
        };
        let detail = pad_or_truncate(&raw_detail, detail_budget);
        let line2 = Line::from(vec![
            Span::raw("    "),
            Span::styled(detail, Style::default().add_modifier(Modifier::DIM)),
        ]);
        return ListItem::new(vec![line1, line2]);
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
    if width >= 80 && cwd_col >= 8 {
        spans.push(Span::raw("  "));
        let cwd_cell = pad_or_truncate(&compact_path(&row.cwd_display), cwd_col);
        spans.push(Span::raw(cwd_cell));
        spans.push(Span::raw("  "));
        let cmd_cell = pad_or_truncate(&row.command_display, cmd_col.max(4));
        spans.push(Span::styled(
            cmd_cell,
            Style::default().add_modifier(Modifier::DIM),
        ));
    } else {
        // 60..80: no cwd column; command and status only.
        spans.push(Span::raw("  "));
        let cmd_cell = pad_or_truncate(&row.command_display, 24);
        spans.push(Span::styled(
            cmd_cell,
            Style::default().add_modifier(Modifier::DIM),
        ));
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
    ListItem::new(Line::from(spans))
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
        Style::default().add_modifier(Modifier::DIM).fg(Color::DarkGray),
    );
    let mut lines = vec![
        Line::from(vec![
            Span::styled("  name:    ", name_style),
            Span::styled(st.name.clone(), Style::default().add_modifier(Modifier::BOLD)),
            if st.stage == AddStage::Name {
                caret.clone()
            } else {
                Span::raw("")
            },
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  command: ", cmd_style),
            Span::styled(st.command.clone(), Style::default().add_modifier(Modifier::BOLD)),
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
                })
                .collect(),
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
            }],
        };
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
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
            AppOutcome::Launch(LaunchKind::Attach { mpx_name }) => {
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
            AppOutcome::Launch(LaunchKind::Attach { mpx_name }) => {
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
        // Labels also appear.
        let body = format!("{row1}\n{row2}\n{row3}");
        assert!(body.contains("[live]"));
        assert!(body.contains("[idle]"));
        assert!(body.contains("[untracked]"));
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
                })
                .collect(),
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
        assert_eq!(app.adding_session.as_ref().unwrap().stage, AddStage::Command);
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
        assert_eq!(app.adding_session.as_ref().unwrap().stage, AddStage::Command);
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
}
