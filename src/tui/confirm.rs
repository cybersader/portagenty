//! Confirm modal used for destructive actions in the TUI. Displays a
//! small centered dialog with a title, a body paragraph, and a
//! `[y]es / [n]o (default: no)` prompt. Callers track the modal's
//! open state (usually via a `pending_action: Option<PendingAction>`
//! field) and decide what to do on confirmation.
//!
//! Contract:
//! - Safe-by-default: Enter without typing anything is the same as
//!   pressing `n`. Destructive actions require an explicit `y` / `Y`.
//! - Any non-y key closes the modal without action. Esc is the
//!   canonical cancel; `n` is also accepted for muscle memory.
//! - The caller owns the action payload; this module only renders
//!   and returns a decision.

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};

/// Outcome of a keystroke while the modal is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKey {
    /// User pressed y/Y — caller should perform the action.
    Confirm,
    /// User pressed anything else — caller should drop the pending
    /// action silently. Esc / n / N / Enter all land here.
    Cancel,
}

/// Decide what a single key press means inside the modal. Kept pure
/// so it's trivial to unit-test without a ratatui backend.
pub fn classify(code: crossterm::event::KeyCode) -> ConfirmKey {
    use crossterm::event::KeyCode;
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => ConfirmKey::Confirm,
        _ => ConfirmKey::Cancel,
    }
}

/// Render a non-destructive *info* modal — same centered, bordered
/// presentation as the confirm modal but without the y/N prompt.
/// Caller decides what dismisses it (typically Esc / q / any key).
/// Used for "reveal path" so users can long-press to select on
/// mobile without the modal vanishing under them.
///
/// Width hugs the longest line of `body` so a mobile long-press
/// doesn't pull in trailing blank cells. No Wrap widget — the
/// caller is expected to pre-wrap content to fit (we cap the
/// computed width at terminal-width-minus-4 anyway, so anything
/// too wide gets truncated rather than wrapped-with-padding).
pub fn render_info(frame: &mut Frame<'_>, area: Rect, title: &str, body: Vec<Line<'static>>) {
    let w = area.width;
    let h = area.height;
    // Longest body line drives the overlay width so trailing cells
    // don't get added to the user's clipboard selection.
    let max_line = body
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    // +2 for the L/R border + 2 for breathing room.
    let want_w = (max_line as u16).saturating_add(4);
    let overlay_w = want_w
        .max(title.len() as u16 + 6) // accommodate the title
        .min(w.saturating_sub(2))
        .max(24)
        .min(w);
    // 2 borders + body height.
    let want_h: u16 = (body.len() as u16) + 2;
    let overlay_h = want_h.min(h.saturating_sub(2)).max(5).min(h);
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
        .title(format!(" {title} "))
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    // No `wrap` — caller pre-wraps; this avoids ratatui's wrap widget
    // padding inserted blank cells past the line content.
    frame.render_widget(Paragraph::new(body).block(block), region);
}

/// Render a confirm modal centered in `area`. `title` is short
/// (fits in the border's title slot); `body` is the 1–3-sentence
/// description of what's about to happen. The y/n prompt is shown
/// in two places — in the title (always visible regardless of
/// body length) and as a styled bottom line in the body — so a
/// long wrapped body can never push the prompt off the visible
/// region.
pub fn render(frame: &mut Frame<'_>, area: Rect, title: &str, body: &str) {
    let w = area.width;
    let h = area.height;
    let overlay_w = w.saturating_sub(4).clamp(28, 64).min(w);

    // Pre-wrap the body so we can size the overlay's height to fit
    // exactly the wrapped content + spacer + prompt line + borders.
    // Wrap budget = inner width = overlay_w - 4 (2 borders + 2 pad).
    let inner_w = overlay_w.saturating_sub(4) as usize;
    let body_lines = wrap_to_width(body, inner_w.max(10));
    // 2 borders + body lines + 1 spacer + 1 prompt line + 1 trailing pad.
    let want_h = (body_lines.len() as u16).saturating_add(5);
    let overlay_h = want_h.min(h.saturating_sub(2)).max(6).min(h);

    let x = area.x + (w.saturating_sub(overlay_w)) / 2;
    let y = area.y + (h.saturating_sub(overlay_h)) / 2;
    let region = Rect {
        x,
        y,
        width: overlay_w,
        height: overlay_h,
    };

    frame.render_widget(Clear, region);

    // Prompt-in-title means even if the body still gets clipped at
    // a brutally small terminal height, the user always sees how to
    // confirm or cancel.
    let title_span = format!(" {title} — [y/N] ");
    let block = Block::default()
        .title(title_span)
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let mut content: Vec<Line<'static>> = Vec::with_capacity(body_lines.len() + 3);
    for line in body_lines {
        content.push(Line::from(vec![Span::raw("  "), Span::raw(line)]));
    }
    content.push(Line::raw(""));
    content.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "y",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" confirm  "),
        Span::styled(
            "n",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" / "),
        Span::styled(
            "Esc",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" cancel  "),
        Span::styled(
            "(default: no)",
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]));

    // No `wrap` widget — body is hand-wrapped above; this avoids
    // ratatui's wrap inserting trailing padding that'd push our
    // prompt line out of view at narrow widths.
    frame.render_widget(Paragraph::new(content).block(block), region);
}

/// Break `text` into lines no longer than `width` chars, splitting
/// on whitespace where possible. Pure utility — exposed for use by
/// the confirm modal sizer; could move to a `tui::wrap` module
/// later if other callers want it.
fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let needed = if current.is_empty() {
            word.chars().count()
        } else {
            current.chars().count() + 1 + word.chars().count()
        };
        if needed > width && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            current.push_str(word);
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn y_confirms_capital_or_lowercase() {
        assert_eq!(classify(KeyCode::Char('y')), ConfirmKey::Confirm);
        assert_eq!(classify(KeyCode::Char('Y')), ConfirmKey::Confirm);
    }

    #[test]
    fn n_cancels() {
        assert_eq!(classify(KeyCode::Char('n')), ConfirmKey::Cancel);
        assert_eq!(classify(KeyCode::Char('N')), ConfirmKey::Cancel);
    }

    #[test]
    fn enter_cancels_defaulting_to_no() {
        // Safety-by-default: a stray Enter should not delete anything.
        assert_eq!(classify(KeyCode::Enter), ConfirmKey::Cancel);
    }

    #[test]
    fn esc_cancels() {
        assert_eq!(classify(KeyCode::Esc), ConfirmKey::Cancel);
    }

    #[test]
    fn renders_without_panic_in_tiny_terminal() {
        let backend = TestBackend::new(30, 12);
        let mut t = Terminal::new(backend).unwrap();
        t.draw(|f| {
            render(
                f,
                f.area(),
                "Delete?",
                "Remove session 'shell' from workspace?",
            )
        })
        .unwrap();
    }
}
