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
use crate::mux::Multiplexer;

/// The reason [`App::run`] returned. The outer entry point uses this
/// to decide whether to exit silently or hand off to the multiplexer
/// to actually launch a session.
#[derive(Debug, Clone)]
pub enum AppOutcome {
    Quit,
    Launch(Session),
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
    list_state: ListState,
    should_quit: bool,
}

impl App {
    pub fn new(workspace: Workspace, mux: Box<dyn Multiplexer>) -> Self {
        let mut list_state = ListState::default();
        if !workspace.sessions.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            workspace,
            mux,
            list_state,
            should_quit: false,
        }
    }

    /// Currently-selected session index, if any. `None` when the
    /// workspace has no sessions.
    pub fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }

    fn select_next(&mut self) {
        let n = self.workspace.sessions.len();
        if n == 0 {
            return;
        }
        let sel = self.list_state.selected().unwrap_or(0);
        self.list_state.select(Some((sel + 1) % n));
    }

    fn select_prev(&mut self) {
        let n = self.workspace.sessions.len();
        if n == 0 {
            return;
        }
        let sel = self.list_state.selected().unwrap_or(0);
        let next = if sel == 0 { n - 1 } else { sel - 1 };
        self.list_state.select(Some(next));
    }

    fn select_first(&mut self) {
        if !self.workspace.sessions.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        let n = self.workspace.sessions.len();
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
            Action::LaunchSelected => self
                .selected()
                .map(|i| AppOutcome::Launch(self.workspace.sessions[i].clone())),
        }
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

        let title = format!(
            " {}  ·  {} session{} ",
            self.workspace.name,
            self.workspace.sessions.len(),
            if self.workspace.sessions.len() == 1 {
                ""
            } else {
                "s"
            },
        );
        let header = Paragraph::new(title).style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_widget(header, chunks[0]);

        self.render_session_list(frame, chunks[1]);

        let footer_text = " j/k: nav · g/G: top/bottom · Enter: launch · q: quit ";
        let footer =
            Paragraph::new(footer_text).style(Style::default().add_modifier(Modifier::DIM));
        frame.render_widget(footer, chunks[2]);
    }

    fn render_session_list(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if self.workspace.sessions.is_empty() {
            let empty = Paragraph::new(" No sessions defined in this workspace. ")
                .style(Style::default().add_modifier(Modifier::DIM));
            frame.render_widget(empty, area);
            return;
        }

        let name_col = self
            .workspace
            .sessions
            .iter()
            .map(|s| s.name.chars().count())
            .max()
            .unwrap_or(0)
            .min(24);

        let items: Vec<ListItem> = self
            .workspace
            .sessions
            .iter()
            .map(|s| session_row(s, name_col))
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

fn session_row<'a>(session: &'a Session, name_col: usize) -> ListItem<'a> {
    let padded_name = if session.name.chars().count() >= name_col {
        session.name.clone()
    } else {
        format!(
            "{:<width$}",
            session.name,
            width = name_col.saturating_add(1)
        )
    };

    ListItem::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(padded_name, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::raw(session.cwd.display().to_string()),
        Span::raw("  "),
        Span::styled(
            &session.command,
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]))
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
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
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
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
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
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
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
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
        let _ = render_to_backend(&mut app, 20, 10);
    }

    #[test]
    fn handles_very_short_terminal() {
        // Minimum: header + one row for body + footer = 3 rows.
        let ws = sample_workspace("tiny", 0);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
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
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
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
            }],
        };
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
        let terminal = render_to_backend(&mut app, 100, 5);

        let body = line_at(&terminal, 1);
        assert!(body.contains("claude"), "name missing: {body:?}");
        assert!(body.contains("/tmp/demo"), "cwd missing: {body:?}");
        assert!(body.contains("--resume"), "command missing: {body:?}");
    }

    #[test]
    fn empty_workspace_shows_placeholder() {
        let ws = sample_workspace("empty", 0);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
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
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
        let _ = render_to_backend(&mut app, 80, 20);
    }

    #[test]
    fn selection_starts_at_zero_for_non_empty() {
        let ws = sample_workspace("x", 3);
        let app = App::new(ws, Box::new(MockMultiplexer::new()));
        assert_eq!(app.selected(), Some(0));
    }

    #[test]
    fn selection_is_none_for_empty_workspace() {
        let ws = sample_workspace("x", 0);
        let app = App::new(ws, Box::new(MockMultiplexer::new()));
        assert_eq!(app.selected(), None);
    }

    #[test]
    fn j_key_advances_selection_wrapping() {
        let ws = sample_workspace("x", 3);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
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
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(2), "should wrap to last");
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(1));
    }

    #[test]
    fn arrow_keys_are_equivalent_to_jk() {
        let ws = sample_workspace("x", 4);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(2));
        app.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(1));
    }

    #[test]
    fn g_goes_to_top_capital_g_to_bottom() {
        let ws = sample_workspace("x", 5);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
        app.handle_key(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(app.selected(), Some(4));
        app.handle_key(KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(app.selected(), Some(0));
    }

    #[test]
    fn navigation_is_noop_on_empty_workspace() {
        let ws = sample_workspace("x", 0);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(app.selected(), None);
    }

    #[test]
    fn enter_returns_launch_action_with_selected_index() {
        let ws = sample_workspace("x", 3);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
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
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
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
            let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
            let action = app.handle_key(key.0, key.1);
            assert_eq!(action, Action::Quit, "key {key:?} should return Quit");
        }
    }

    #[test]
    fn highlight_symbol_appears_next_to_selected_row() {
        let ws = sample_workspace("x", 3);
        let mut app = App::new(ws, Box::new(MockMultiplexer::new()));
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
}
