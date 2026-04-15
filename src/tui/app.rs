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
    Create { session: Session },
    /// Already live (workspace or untracked): `attach` by sanitized name.
    Attach { mpx_name: String },
}

/// The reason [`App::run`] returned. The outer entry point uses this
/// to decide whether to exit silently or hand off to the multiplexer.
#[derive(Debug, Clone)]
pub enum AppOutcome {
    Quit,
    Launch(LaunchKind),
}

/// Internal action dispatch. Returned from [`App::handle_key`] so the
/// event loop can translate a key press into either continued
/// in-TUI work or a reason to exit the loop.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    LaunchSelected,
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
            terminal.draw(|frame| self.render(frame))?;
            if let Some(outcome) = self.handle_event()? {
                return Ok((outcome, self.mux));
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
            Action::LaunchSelected => self.selected().and_then(|i| {
                let row = self.rows.get(i)?;
                let kind = match row.state {
                    SessionState::NotStarted => row
                        .session
                        .as_ref()
                        .map(|s| LaunchKind::Create { session: s.clone() })?,
                    SessionState::Live | SessionState::Untracked => LaunchKind::Attach {
                        mpx_name: row.mpx_name.clone(),
                    },
                };
                Some(AppOutcome::Launch(kind))
            }),
        }
    }

    /// The currently-selected row, if any. Exposed so the outer entry
    /// point can ask "what did the user pick?" after `run` returns.
    pub fn selected_row(&self) -> Option<&SessionRow> {
        self.selected().and_then(|i| self.rows.get(i))
    }

    /// Apply a single key press, returning whatever [`Action`] it
    /// produced. Split from `handle_event` so tests drive input
    /// synchronously without faking a crossterm event stream.
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Action {
        match (code, mods) {
            (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => {
                self.should_quit = true;
                Action::Quit
            }
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                Action::Quit
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
            _ => Action::None,
        }
    }

    /// Render a single frame. Pulled out so tests can call it against
    /// a `TestBackend` without needing the event loop.
    pub fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        let tracked = self.workspace.sessions.len();
        let untracked = self
            .rows
            .iter()
            .filter(|r| r.state == SessionState::Untracked)
            .count();
        let title = if untracked > 0 {
            format!(
                " {}  ·  {} session{}  · {} untracked ",
                self.workspace.name,
                tracked,
                if tracked == 1 { "" } else { "s" },
                untracked,
            )
        } else {
            format!(
                " {}  ·  {} session{} ",
                self.workspace.name,
                tracked,
                if tracked == 1 { "" } else { "s" },
            )
        };
        let header = Paragraph::new(title).style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_widget(header, chunks[0]);

        self.render_session_list(frame, chunks[1]);

        let footer_text = footer_for_width(area.width);
        let footer =
            Paragraph::new(footer_text).style(Style::default().add_modifier(Modifier::DIM));
        frame.render_widget(footer, chunks[2]);
    }

    fn render_session_list(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if self.rows.is_empty() {
            let empty = Paragraph::new(" No sessions defined or running. ")
                .style(Style::default().add_modifier(Modifier::DIM));
            frame.render_widget(empty, area);
            return;
        }

        let name_col = self
            .rows
            .iter()
            .map(|r| r.display_name.chars().count())
            .max()
            .unwrap_or(0)
            .min(24);

        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|r| row_list_item(r, name_col))
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

/// Tiered footer text. Narrow terminals (phone-in-portrait over SSH
/// from Termux) get a shorter hint so "quit" is always visible. See
/// DESIGN.md §10 for the mobile constraints that drive this.
fn footer_for_width(width: u16) -> &'static str {
    if width >= 60 {
        " j/k: nav · g/G: top/bottom · Enter: launch · q: quit "
    } else if width >= 30 {
        " j/k · Enter: launch · q: quit "
    } else {
        " q: quit "
    }
}

fn row_list_item(row: &SessionRow, name_col: usize) -> ListItem<'static> {
    let padded_name = if row.display_name.chars().count() >= name_col {
        row.display_name.clone()
    } else {
        format!(
            "{:<width$}",
            row.display_name,
            width = name_col.saturating_add(1)
        )
    };

    // State marker (● ○ ?) — color encodes Live/NotStarted/Untracked.
    let marker_style = match row.state {
        SessionState::Live => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        SessionState::NotStarted => Style::default().add_modifier(Modifier::DIM),
        SessionState::Untracked => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    };

    // Kind marker — small per-kind glyph shown right after the state
    // marker when the session has a kind hint. Covered by v1.x
    // item 9 in ROADMAP.md.
    let (kind_glyph, kind_style) = kind_display(row.kind);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(12);
    spans.push(Span::raw(" "));
    spans.push(Span::styled(row.state.marker().to_string(), marker_style));
    spans.push(Span::raw(" "));
    if let Some(glyph) = kind_glyph {
        spans.push(Span::styled(glyph.to_string(), kind_style));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        padded_name,
        Style::default().add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::raw(row.cwd_display.clone()));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        row.command_display.clone(),
        Style::default().add_modifier(Modifier::DIM),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        format!("[{}]", row.state.label()),
        Style::default().add_modifier(Modifier::DIM),
    ));
    ListItem::new(Line::from(spans))
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
            file_path: None,
            multiplexer: MpxEnum::Tmux,
            projects: vec![],
            sessions: (0..sessions)
                .map(|i| Session {
                    name: format!("s{i}"),
                    cwd: PathBuf::from("/tmp"),
                    command: "true".into(),
                    kind: None,
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
    fn renders_footer_with_quit_hint() {
        let ws = sample_workspace("X", 0);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let terminal = render_to_backend(&mut app, 60, 5);

        let buffer = terminal.backend().buffer();
        let last_line: String = (0..60)
            .map(|x| buffer[(x, 4)].symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(last_line.contains("quit"), "got: {last_line:?}");
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
            file_path: None,
            multiplexer: MpxEnum::Tmux,
            projects: vec![],
            sessions: vec![Session {
                name: "claude".into(),
                cwd: PathBuf::from("/tmp/demo"),
                command: "claude --resume".into(),
                kind: None,
            }],
        };
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let terminal = render_to_backend(&mut app, 100, 5);

        let body = line_at(&terminal, 1);
        assert!(body.contains("claude"), "name missing: {body:?}");
        assert!(body.contains("/tmp/demo"), "cwd missing: {body:?}");
        assert!(body.contains("--resume"), "command missing: {body:?}");
    }

    #[test]
    fn empty_workspace_shows_placeholder() {
        let ws = sample_workspace("empty", 0);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        let terminal = render_to_backend(&mut app, 60, 5);

        let body = line_at(&terminal, 1);
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
    fn quit_keys_return_quit_action() {
        for key in [
            (KeyCode::Char('q'), KeyModifiers::NONE),
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let ws = sample_workspace("x", 2);
            let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
            let action = app.handle_key(key.0, key.1);
            assert_eq!(action, Action::Quit, "key {key:?} should return Quit");
        }
    }

    #[test]
    fn highlight_symbol_appears_next_to_selected_row() {
        let ws = sample_workspace("x", 3);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()), vec![]);
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE); // select index 1
        let terminal = render_to_backend(&mut app, 80, 10);
        // Row 0 is the header; row 1 is the first session (index 0);
        // row 2 is index 1 (selected). The highlight marker is "▶ ".
        let row2 = line_at(&terminal, 2);
        assert!(
            row2.contains("▶"),
            "expected highlight on selected row, got: {row2:?}"
        );
        // Non-selected rows should not have the marker.
        let row1 = line_at(&terminal, 1);
        assert!(
            !row1.contains("▶"),
            "unexpected highlight on row 1: {row1:?}"
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
        // Footer on the last row always has quit hint.
        let footer = line_at(&terminal, h - 1);
        assert!(
            footer.to_lowercase().contains("quit"),
            "footer missing at {w}x{h}: {footer:?}"
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

    fn live_session(name: &str) -> SessionInfo {
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
            vec![live_session("stranger")],
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
            AppOutcome::Launch(LaunchKind::Create { session }) => {
                assert_eq!(session.name, "s0");
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
                assert_eq!(mpx_name, "s0");
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
            vec![live_session("orphan-session")],
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
            vec![live_session("s0"), live_session("extra")],
        );
        let terminal = render_to_backend(&mut app, 100, 10);
        // Body spans rows 1..h-1. Three rows expected in order:
        // row 1 = s0 (live ●), row 2 = s1 (not-started ○),
        // row 3 = extra (untracked ?).
        let row1 = line_at(&terminal, 1);
        let row2 = line_at(&terminal, 2);
        let row3 = line_at(&terminal, 3);
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
            vec![live_session("other"), live_session("another")],
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

        // Rows 1..=7 correspond to the 7 sessions we defined.
        let row_for = |idx: u16| line_at(&terminal, idx);
        assert!(
            row_for(1).contains(" C "),
            "claude row missing C: {:?}",
            row_for(1)
        );
        assert!(
            row_for(2).contains(" O "),
            "opencode row missing O: {:?}",
            row_for(2)
        );
        assert!(
            row_for(3).contains(" E "),
            "editor row missing E: {:?}",
            row_for(3)
        );
        assert!(
            row_for(4).contains(" D "),
            "dev-server row missing D: {:?}",
            row_for(4)
        );
        // Shell/Other/None → no kind marker. Check that the row
        // doesn't stray into another kind's letter.
        for (idx, name) in [(5u16, "shell"), (6, "other"), (7, "notype")] {
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
}
