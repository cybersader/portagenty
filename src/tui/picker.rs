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

/// Run the picker inside an already-initialized ratatui terminal.
/// Terminal init + restore stay with the caller so a single
/// `ratatui::init()` handles both the picker and the session-list
/// TUI that follows — no flicker from tearing down between them.
pub fn run(terminal: &mut DefaultTerminal, workspaces: &[PathBuf]) -> Result<PickerOutcome> {
    let mut state = ListState::default();
    state.select(Some(0));

    let total = workspaces.len() + 1; // +1 for the "live sessions" row
    let mut help_open = false;

    loop {
        terminal.draw(|frame| render(frame, workspaces, &mut state, help_open))?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        // Help overlay: any key closes it. No passthrough — we don't
        // want `Enter` on a "dismiss help" press to also open a
        // workspace the user hadn't seen yet.
        if help_open {
            help_open = false;
            continue;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('?'), _) => {
                help_open = true;
            }
            (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return Ok(PickerOutcome::Quit),
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
            (KeyCode::Enter, _) => {
                let sel = state.selected().unwrap_or(0);
                if sel == workspaces.len() {
                    // Last row is the "live sessions" sentinel.
                    return Ok(PickerOutcome::LiveBrowse);
                }
                return Ok(PickerOutcome::Workspace(workspaces[sel].clone()));
            }
            _ => {}
        }
    }
}

fn render(frame: &mut Frame<'_>, workspaces: &[PathBuf], state: &mut ListState, help_open: bool) {
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

    let footer = Paragraph::new(" j/k · Enter: open · ?: help · q: quit ")
        .style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(footer, chunks[3]);

    if help_open {
        crate::tui::help::render_overlay(frame, area, crate::tui::help::HelpContext::Picker);
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
