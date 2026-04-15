//! Workspace picker TUI. Runs before the session-list TUI when the
//! user invokes `pa` from a directory with no walkable workspace but
//! has registered workspaces globally. Keeps the UI consistent —
//! everything is rendered via ratatui, no stdin text prompts.
//!
//! Intentionally tiny: own event loop, own render, no sharing with
//! `app::App`. The two screens have different data shapes (workspaces
//! vs sessions) so folding them into one widget would mean more
//! conditionals than code. Keeping them separate is easier to read.

use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    prelude::*,
    widgets::{List, ListItem, ListState, Paragraph},
    DefaultTerminal,
};

/// What the picker returned.
#[derive(Debug, Clone)]
pub enum PickerOutcome {
    /// User picked a workspace file. Caller should load it.
    Workspace(PathBuf),
    /// User picked "browse live sessions on this machine".
    LiveBrowse,
    /// User bailed (q / Esc). Caller should exit cleanly.
    Quit,
}

/// How long status messages stick around before auto-clearing.
const STATUS_TTL: std::time::Duration = std::time::Duration::from_millis(2500);

/// Footer status line with auto-clear support.
#[derive(Debug, Default)]
struct StatusLine {
    text: Option<String>,
    set_at: Option<std::time::Instant>,
}

impl StatusLine {
    fn set(&mut self, msg: String) {
        self.text = Some(msg);
        self.set_at = Some(std::time::Instant::now());
    }
    fn clear(&mut self) {
        self.text = None;
        self.set_at = None;
    }
    fn age_out(&mut self) {
        if let Some(set_at) = self.set_at {
            if set_at.elapsed() >= STATUS_TTL {
                self.clear();
            }
        }
    }
}

/// Destructive action awaiting user confirmation in the picker.
#[derive(Debug, Clone)]
enum PickerPending {
    /// Drop the row from the global `[[workspace]]` registry. Leaves
    /// the `.portagenty.toml` file on disk untouched — only the index
    /// loses the entry.
    Unregister(PathBuf),
    /// Delete the workspace file from disk *and* drop the registry
    /// entry. Files-on-disk are gone after this.
    DeleteFile(PathBuf),
}

/// Sticky info modal contents. Distinct from `PickerPending` because
/// info modals are non-destructive and dismissed without a y/N
/// classifier.
#[derive(Debug, Clone)]
struct InfoModal {
    title: String,
    /// Pre-rendered lines so callers can decide colors.
    lines: Vec<Line<'static>>,
}

/// Run the picker inside an already-initialized ratatui terminal.
/// Terminal init + restore stay with the caller so a single
/// `ratatui::init()` handles both the picker and the session-list
/// TUI that follows — no flicker from tearing down between them.
pub fn run(terminal: &mut DefaultTerminal, workspaces: &[PathBuf]) -> Result<PickerOutcome> {
    // Picker-local mutable copy: actions like unregister / delete
    // change the list in place without having to re-enter the outer
    // run loop.
    let mut workspaces: Vec<PathBuf> = workspaces.to_vec();
    let mut state = ListState::default();
    state.select(Some(0));

    let mut help_open = false;
    let mut pending: Option<PickerPending> = None;
    let mut info: Option<InfoModal> = None;
    let mut status = StatusLine::default();

    loop {
        // Auto-age the status line so messages don't sit forever.
        status.age_out();
        let total = workspaces.len() + 1; // +1 for the "live sessions" row
        terminal.draw(|frame| {
            render(
                frame,
                &workspaces,
                &mut state,
                help_open,
                &pending,
                &info,
                &status.text,
            )
        })?;

        if !event::poll(std::time::Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        // Help overlay: any key closes it. No passthrough.
        if help_open {
            help_open = false;
            continue;
        }
        // Info modal: any key closes it. No passthrough — accidental
        // Enter shouldn't open the highlighted workspace.
        if info.is_some() {
            info = None;
            continue;
        }
        // Confirm modal: divert keys.
        if let Some(action) = pending.take() {
            match crate::tui::confirm::classify(key.code) {
                crate::tui::confirm::ConfirmKey::Confirm => {
                    let msg = perform_picker_action(action, &mut workspaces, &mut state);
                    status.set(msg);
                }
                crate::tui::confirm::ConfirmKey::Cancel => {
                    status.set("cancelled".into());
                }
            }
            continue;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('?'), _) => {
                help_open = true;
            }
            (KeyCode::Char('q'), _) => return Ok(PickerOutcome::Quit),
            (KeyCode::Esc, _) => {
                // Two-stage Esc: dismiss a status line first, then
                // exit pa on the second press. Prevents an
                // accidental Esc from quitting after a stray action.
                if status.text.is_some() {
                    status.clear();
                } else {
                    return Ok(PickerOutcome::Quit);
                }
            }
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                return Ok(PickerOutcome::Quit);
            }
            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                let sel = state.selected().unwrap_or(0);
                state.select(Some((sel + 1) % total));
            }
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                let sel = state.selected().unwrap_or(0);
                state.select(Some(if sel == 0 { total - 1 } else { sel - 1 }));
            }
            (KeyCode::Char('g'), _) | (KeyCode::Home, _) => {
                state.select(Some(0));
            }
            (KeyCode::Char('G'), _) | (KeyCode::End, _) => {
                state.select(Some(total - 1));
            }
            (KeyCode::Char('d'), _) => {
                if let Some(path) = selected_workspace(&workspaces, &state) {
                    pending = Some(PickerPending::Unregister(path));
                } else {
                    status.set(
                        "d: nothing to unregister — live-sessions row isn't a workspace".into(),
                    );
                }
            }
            (KeyCode::Char('D'), _) => {
                if let Some(path) = selected_workspace(&workspaces, &state) {
                    pending = Some(PickerPending::DeleteFile(path));
                } else {
                    status.set("D: nothing to delete — live-sessions row isn't a workspace".into());
                }
            }
            (KeyCode::Char('r'), _) => {
                if let Some(path) = selected_workspace(&workspaces, &state) {
                    info = Some(build_reveal_modal(&path));
                } else {
                    status.set("r: live-sessions row has no file path".into());
                }
            }
            (KeyCode::Char('n'), _) => {
                status.set(
                    "n: to scaffold a new workspace, exit pa and run 'pa onboard' or 'pa init <name>'"
                        .into(),
                );
            }
            (KeyCode::Char('e'), _) => {
                status.set("e: in-TUI workspace editing is coming soon".into());
            }
            (KeyCode::Enter, _) => {
                let sel = state.selected().unwrap_or(0);
                if sel == workspaces.len() {
                    return Ok(PickerOutcome::LiveBrowse);
                }
                return Ok(PickerOutcome::Workspace(workspaces[sel].clone()));
            }
            _ => {}
        }
    }
}

/// Build the "reveal path" info modal. Auto-attempts to copy the
/// path to the system clipboard so users can paste it elsewhere
/// (especially mobile, where long-press selection inside the TUI
/// is fiddly).
fn build_reveal_modal(path: &std::path::Path) -> InfoModal {
    let path_str = path.display().to_string();
    let copy_result = crate::clipboard::copy(&path_str);
    let copy_line = match copy_result {
        Ok(tool) => Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "✓ copied",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" to clipboard via `{tool}`")),
        ]),
        Err(e) => Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "couldn't copy:",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                e.to_string().lines().next().unwrap_or("").to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]),
    };
    InfoModal {
        title: "Workspace path".into(),
        lines: vec![
            Line::raw(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(path_str, Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::raw(""),
            copy_line,
            Line::raw(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "Press any key (Esc / q / Enter) to close.",
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]),
        ],
    }
}

/// Resolve the currently-selected row to a workspace PathBuf, or
/// `None` when the live-sessions sentinel is selected. All the
/// row-action keys share this guard so the sentinel is handled
/// consistently.
fn selected_workspace(workspaces: &[PathBuf], state: &ListState) -> Option<PathBuf> {
    let sel = state.selected()?;
    if sel >= workspaces.len() {
        return None;
    }
    workspaces.get(sel).cloned()
}

/// Execute a confirmed picker action and mutate the row list + state
/// in place. Returns a human-readable status string the UI pins in
/// the footer until the next keystroke.
fn perform_picker_action(
    action: PickerPending,
    workspaces: &mut Vec<PathBuf>,
    state: &mut ListState,
) -> String {
    match action {
        PickerPending::Unregister(path) => {
            match crate::config::unregister_global_workspace(&path) {
                Ok(()) => {
                    workspaces.retain(|p| p != &path);
                    clamp_selection(workspaces, state);
                    format!("unregistered from global index: {}", path.display())
                }
                Err(e) => format!("unregister failed: {e:#}"),
            }
        }
        PickerPending::DeleteFile(path) => {
            // Delete the file first; if successful, then drop the
            // registry entry (best-effort — a stale entry is
            // auto-filtered on next picker load anyway).
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    let _ = crate::config::unregister_global_workspace(&path);
                    workspaces.retain(|p| p != &path);
                    clamp_selection(workspaces, state);
                    format!("deleted workspace file: {}", path.display())
                }
                Err(e) => format!("delete failed: {e}"),
            }
        }
    }
}

fn clamp_selection(workspaces: &[PathBuf], state: &mut ListState) {
    let total = workspaces.len() + 1; // + live-sessions sentinel
    let sel = state.selected().unwrap_or(0);
    if total == 0 {
        state.select(None);
    } else {
        state.select(Some(sel.min(total - 1)));
    }
}

fn render(
    frame: &mut Frame<'_>,
    workspaces: &[PathBuf],
    state: &mut ListState,
    help_open: bool,
    pending: &Option<PickerPending>,
    info: &Option<InfoModal>,
    status: &Option<String>,
) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // spacer/hint
            Constraint::Min(0),    // list
            Constraint::Length(1), // footer
        ])
        .split(area);

    let title = Paragraph::new(" portagenty  ·  pick a workspace ")
        .style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(title, chunks[0]);

    let hint = Paragraph::new(" No workspace in this directory — choose one of your registered workspaces, or browse live sessions. ")
        .style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(hint, chunks[1]);

    // Width budget for each row so long paths don't run past the
    // viewport. On narrow terminals we drop the path and show only
    // the name + relative-time hint — the full path belongs in help
    // or details, not in a row that'd truncate awkwardly.
    let row_width = chunks[2].width as usize;
    let mut items: Vec<ListItem> = Vec::with_capacity(workspaces.len() + 1);
    for path in workspaces {
        let label = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".portagenty"))
            .unwrap_or_else(|| path.file_name().and_then(|s| s.to_str()).unwrap_or("?"));
        let dir = path
            .parent()
            .map(|p| compact_path(&p.display().to_string()))
            .unwrap_or_default();
        let relative = crate::state::relative_time(crate::state::last_launch_for_workspace(path));

        if row_width >= 70 {
            // Wide: name · path · time. Path middle-truncates to fit.
            let name_budget = label.chars().count().min(22);
            // Remaining after: gutter(6) + name + sep(3) + time(12) + pad(2)
            let used = 6 + name_budget + 3 + 12 + 2;
            let path_budget = row_width.saturating_sub(used).clamp(10, 50);
            items.push(ListItem::new(Line::from(vec![
                Span::raw(" "),
                Span::styled("●", Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(
                    label.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled(
                    truncate_middle(&dir, path_budget),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::raw("   "),
                Span::styled(relative, Style::default().add_modifier(Modifier::DIM)),
            ])));
        } else {
            // Narrow / Termux portrait: two-line card. Line 1 name +
            // relative time; line 2 indented dim path with middle
            // truncation so it can't overflow.
            let path_budget = row_width.saturating_sub(6).max(10);
            items.push(ListItem::new(vec![
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled("●", Style::default().fg(Color::Cyan)),
                    Span::raw("  "),
                    Span::styled(
                        label.to_string(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(relative, Style::default().add_modifier(Modifier::DIM)),
                ]),
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        truncate_middle(&dir, path_budget),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ]),
            ]));
        }
    }
    // Sentinel row: live browse.
    items.push(ListItem::new(Line::from(vec![
        Span::raw(" "),
        Span::styled("…", Style::default().add_modifier(Modifier::DIM)),
        Span::raw("  "),
        Span::styled(
            "live sessions on this machine",
            Style::default()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::DIM),
        ),
        Span::raw("   "),
        Span::styled(
            "(no workspace — just attach to what's running)",
            Style::default().add_modifier(Modifier::DIM),
        ),
    ])));

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, chunks[2], state);

    if let Some(s) = status {
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                s.clone(),
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
    } else {
        let footer = Paragraph::new(" j/k · Enter · d/D/r · ?: help · q: quit ")
            .style(Style::default().add_modifier(Modifier::DIM));
        frame.render_widget(footer, chunks[3]);
    }

    if let Some(p) = pending {
        let (title, body) = picker_confirm_copy(p);
        crate::tui::confirm::render(frame, area, &title, &body);
    }

    if let Some(modal) = info {
        crate::tui::confirm::render_info(frame, area, &modal.title, modal.lines.clone());
    }

    if help_open {
        crate::tui::help::render_overlay(frame, area, crate::tui::help::HelpContext::Picker);
    }
}

fn picker_confirm_copy(p: &PickerPending) -> (String, String) {
    match p {
        PickerPending::Unregister(path) => (
            "Unregister workspace".into(),
            format!(
                "Drop this workspace from the global picker index? \
                 The file {} stays on disk; you can re-register with \
                 `pa init` or by running `pa onboard` here.",
                path.display(),
            ),
        ),
        PickerPending::DeleteFile(path) => (
            "Delete workspace file".into(),
            format!(
                "DELETE THE FILE {} from disk and remove it from the global \
                 picker index? This is destructive — the workspace TOML \
                 cannot be recovered unless you have a backup or git history.",
                path.display(),
            ),
        ),
    }
}

/// Middle-ellipsis truncation for paths. Keeps the start + end and
/// drops the middle; biased toward preserving the tail because the
/// project leaf is more recognizable than the ancestor directories.
fn truncate_middle(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max <= 1 {
        return s.chars().take(max).collect();
    }
    let ell = "…";
    let keep = max - 1;
    let tail = (keep * 2).div_ceil(3);
    let head = keep - tail;
    let head_str: String = s.chars().take(head).collect();
    let tail_start = count - tail;
    let tail_str: String = s.chars().skip(tail_start).collect();
    format!("{head_str}{ell}{tail_str}")
}

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
