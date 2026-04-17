//! Full-screen help overlay for the TUI. Invoked with `?` from
//! either the picker or the session-list screen. Fixes the
//! "narrow-terminal footer is truncated — how do I quit?" problem:
//! the footer is a one-line reminder, not the source of truth.
//!
//! Design:
//! - Single rendered frame; no input state of its own. Callers track
//!   `help_open: bool`, render this overlay instead of their normal
//!   content when true, and toggle off on any key press.
//! - Key table is authored in the source, not derived from the event
//!   loop, because the event loop's `handle_key` knows what a key
//!   *does*, not what to *call* it for humans.

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

/// Screen on which the help overlay is shown. Keys differ per
/// screen, so we tailor the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpContext {
    Picker,
    SessionList,
}

/// Render the overlay centered in `area`. Caller is responsible for
/// drawing it *after* the underlying screen so it sits on top; the
/// internal `Clear` widget wipes the region first so stacked content
/// isn't visible through it.
pub fn render_overlay(frame: &mut Frame<'_>, area: Rect, ctx: HelpContext) {
    // Centered box, generous on wide screens, full-width on narrow.
    let w = area.width;
    let h = area.height;
    let overlay_w = w.saturating_sub(4).clamp(20, 70).min(w);
    // Enough height on a desktop to show all sections (keys + markers
    // + kind glyphs + title-bar + coming-soon) without clipping.
    // Narrow terminals (Termux portrait) clamp to available height.
    let overlay_h = h.saturating_sub(2).clamp(10, 40).min(h);
    let x = area.x + (w.saturating_sub(overlay_w)) / 2;
    let y = area.y + (h.saturating_sub(overlay_h)) / 2;
    let region = Rect {
        x,
        y,
        width: overlay_w,
        height: overlay_h,
    };

    frame.render_widget(Clear, region);

    let body = help_body(ctx);
    let block = Block::default()
        .title(" help ")
        .title_style(Style::default().add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(
        Paragraph::new(body).block(block).wrap(Wrap { trim: false }),
        region,
    );
}

/// Plain-text help content. Kept in source so translation / tone
/// changes are one-file edits.
fn help_body(ctx: HelpContext) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(24);

    let heading = |s: &'static str| {
        Line::from(Span::styled(
            s,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let row = |key: &'static str, what: &'static str| {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{key:<14}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(what),
        ])
    };

    match ctx {
        HelpContext::Picker => {
            lines.push(heading(" Workspace picker — navigation"));
            lines.push(Line::raw(""));
            lines.push(row("j / ↓", "down"));
            lines.push(row("k / ↑", "up"));
            lines.push(row("g / Home", "first"));
            lines.push(row("G / End", "last"));
            lines.push(row("Ctrl+D / Ctrl+U", "half-page down / up"));
            lines.push(row("PgDn / PgUp", "10-row jumps"));
            lines.push(row("l / → / Enter", "open the highlighted workspace"));
            lines.push(row("Esc / q", "exit pa"));
            lines.push(row("Ctrl+C", "exit pa"));
            lines.push(row("?", "toggle this help"));
            lines.push(Line::raw(""));
            lines.push(heading(" Workspace actions"));
            lines.push(Line::raw(""));
            lines.push(row("n", "find folder + scaffold new workspace"));
            lines.push(row("R", "rename workspace (edits TOML name)"));
            lines.push(row("d", "unregister from global index (file stays)"));
            lines.push(row("D", "delete workspace file (destructive)"));
            lines.push(row("r", "reveal: show file path in modal"));
        }
        HelpContext::SessionList => {
            lines.push(heading(" Session list — navigation"));
            lines.push(Line::raw(""));
            lines.push(row("j / ↓", "down"));
            lines.push(row("k / ↑", "up"));
            lines.push(row("g / Home", "first"));
            lines.push(row("G / End", "last"));
            lines.push(row("Ctrl+D / Ctrl+U", "half-page down / up"));
            lines.push(row("PgDn / PgUp", "10-row jumps"));
            lines.push(row("l / → / Enter", "attach (or create-and-attach)"));
            lines.push(row("Esc / q / Ctrl+Q", "back to picker (home screen)"));
            lines.push(row("Ctrl+C", "exit pa"));
            lines.push(row("?", "toggle this help"));
            lines.push(Line::raw(""));
            lines.push(heading(" Row markers"));
            lines.push(Line::raw(""));
            lines.push(row("●  live", "session is running in the multiplexer"));
            lines.push(row("○  idle", "declared in the workspace, not started yet"));
            lines.push(row(
                "?  untracked",
                "live mpx session without a workspace entry",
            ));
            lines.push(Line::raw(""));
            lines.push(heading(" Kind glyphs"));
            lines.push(Line::raw(""));
            lines.push(row("C (blue)", "claude-code"));
            lines.push(row("O (cyan)", "opencode"));
            lines.push(row("E (magenta)", "editor"));
            lines.push(row("D (green)", "dev-server"));
            lines.push(row("(none)", "shell / other / no hint"));
            lines.push(Line::raw(""));
            lines.push(heading(" Title bar"));
            lines.push(Line::raw(""));
            lines.push(row("[tmux]", "cyan badge — tmux multiplexer"));
            lines.push(row("[zellij]", "magenta badge — zellij multiplexer"));
            lines.push(row(
                "N untracked",
                "yellow — live mpx sessions outside the workspace",
            ));
            lines.push(Line::raw(""));
            lines.push(heading(" Row + workspace actions"));
            lines.push(Line::raw(""));
            lines.push(row("a", "add a new session to the workspace"));
            lines.push(row("e", "edit session field (rename/cwd/cmd/kind/env)"));
            lines.push(row("d", "delete the session (edits TOML)"));
            lines.push(row("x", "kill the live mpx session"));
            lines.push(row("m", "switch workspace multiplexer (tmux ↔ zellij)"));
            lines.push(Line::raw(""));
            lines.push(heading(" Coming soon"));
            lines.push(Line::raw(""));
            lines.push(row("Space", "context menu for the row"));
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn buffer_text(t: &Terminal<TestBackend>) -> String {
        let buf = t.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area().height {
            for x in 0..buf.area().width {
                out.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn session_list_help_mentions_esc_back_and_q_quit() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_overlay(f, f.area(), HelpContext::SessionList))
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Esc"), "help should mention Esc:\n{text}");
        assert!(
            text.contains("back to picker"),
            "help should explain Esc:\n{text}"
        );
        assert!(
            text.contains("exit pa"),
            "help should explain quit:\n{text}"
        );
    }

    #[test]
    fn picker_help_mentions_esc_q_exits() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_overlay(f, f.area(), HelpContext::Picker))
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(
            text.contains("Esc / q"),
            "picker help should list Esc/q:\n{text}"
        );
        assert!(
            text.contains("open the highlighted"),
            "picker help should explain Enter:\n{text}"
        );
    }

    #[test]
    fn help_renders_inside_small_areas_without_panic() {
        // Termux portrait size — verify no slicing panic.
        let backend = TestBackend::new(30, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_overlay(f, f.area(), HelpContext::SessionList))
            .unwrap();
    }

    #[test]
    fn help_renders_inside_tiny_areas_without_panic() {
        // Below the minimum we'd ever see in practice.
        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_overlay(f, f.area(), HelpContext::Picker))
            .unwrap();
    }
}
