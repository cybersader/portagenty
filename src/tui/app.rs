//! TUI app state + render loop. Ratatui 0.29 + crossterm 0.28.
//!
//! v1 renders a single-column session list over the resolved
//! `domain::Workspace`. Two-pane project/session layouts and the
//! Tags / Custom Groups views come in v1.x per `ROADMAP.md`.

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    prelude::*,
    widgets::{List, ListItem, Paragraph},
    DefaultTerminal,
};

use crate::domain::{Session, Workspace};
use crate::mux::Multiplexer;

/// Top-level TUI state. Holds everything the event loop needs; no
/// globals, nothing static. Tests construct `App` directly and render
/// into a `ratatui::backend::TestBackend`.
pub struct App {
    workspace: Workspace,
    #[allow(dead_code)] // wired to mux in a later commit
    mux: Box<dyn Multiplexer>,
    should_quit: bool,
}

impl App {
    pub fn new(workspace: Workspace, mux: Box<dyn Multiplexer>) -> Self {
        Self {
            workspace,
            mux,
            should_quit: false,
        }
    }

    /// Run the event loop until the user quits. Owns the terminal for
    /// its duration; restores it on drop (ratatui's `restore` is called
    /// by the caller via `DefaultTerminal::drop`).
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.should_quit {
            terminal.draw(|frame| self.render(frame))?;
            self.handle_event()?;
        }
        Ok(())
    }

    fn handle_event(&mut self) -> Result<()> {
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }
            match (key.code, key.modifiers) {
                (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => self.should_quit = true,
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Render a single frame. Pulled out so tests can call it against
    /// a `TestBackend` without needing the event loop.
    pub fn render(&self, frame: &mut Frame<'_>) {
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

        let footer_text = " q / Esc: quit ";
        let footer =
            Paragraph::new(footer_text).style(Style::default().add_modifier(Modifier::DIM));
        frame.render_widget(footer, chunks[2]);
    }

    fn render_session_list(&self, frame: &mut Frame<'_>, area: Rect) {
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

        let list = List::new(items);
        frame.render_widget(list, area);
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

    fn render_to_backend(app: &App, w: u16, h: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        terminal
    }

    #[test]
    fn renders_header_with_workspace_name_and_session_count() {
        let ws = sample_workspace("Agentic", 3);
        let app = App::new(ws, Box::new(MockMultiplexer::new()));
        let terminal = render_to_backend(&app, 60, 10);

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
        let app = App::new(ws, Box::new(MockMultiplexer::new()));
        let terminal = render_to_backend(&app, 60, 10);

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
        let app = App::new(ws, Box::new(MockMultiplexer::new()));
        let terminal = render_to_backend(&app, 60, 5);

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
        let app = App::new(ws, Box::new(MockMultiplexer::new()));
        let _ = render_to_backend(&app, 20, 10);
    }

    #[test]
    fn handles_very_short_terminal() {
        // Minimum: header + one row for body + footer = 3 rows.
        let ws = sample_workspace("tiny", 0);
        let app = App::new(ws, Box::new(MockMultiplexer::new()));
        let _ = render_to_backend(&app, 80, 3);
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
        let app = App::new(ws, Box::new(MockMultiplexer::new()));
        let terminal = render_to_backend(&app, 100, 10);

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
        let app = App::new(ws, Box::new(MockMultiplexer::new()));
        let terminal = render_to_backend(&app, 100, 5);

        let body = line_at(&terminal, 1);
        assert!(body.contains("claude"), "name missing: {body:?}");
        assert!(body.contains("/tmp/demo"), "cwd missing: {body:?}");
        assert!(body.contains("--resume"), "command missing: {body:?}");
    }

    #[test]
    fn empty_workspace_shows_placeholder() {
        let ws = sample_workspace("empty", 0);
        let app = App::new(ws, Box::new(MockMultiplexer::new()));
        let terminal = render_to_backend(&app, 60, 5);

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
        let app = App::new(ws, Box::new(MockMultiplexer::new()));
        let _ = render_to_backend(&app, 80, 20);
    }
}
