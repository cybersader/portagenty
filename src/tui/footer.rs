//! Responsive keybind hint footer. Replaces the operators-only
//! string footers that lived inline in `app.rs::footer_for_width`
//! and the picker's hardcoded footer line — both lost meaning at
//! narrow widths because the *labels* were the first thing dropped.
//!
//! Model:
//! - Caller hands us a slice of `(key, label)` pairs in priority
//!   order (most-important first).
//! - We render them as ` <key> <label> · <key> <label> · …`, with
//!   `key` cyan-bold, `label` dim, and `·` as the separator.
//! - As the available width shrinks, we drop the *least*-important
//!   pairs first; if width is brutally narrow, we drop labels too
//!   and show keys-only.
//!
//! This keeps the most useful keys (typically `?` for help and `q`
//! for quit) visible at every width, with progressive disclosure
//! of richer labels as width allows.

use ratatui::{prelude::*, widgets::Paragraph};

/// One footer entry. `key` is the printable form of the keystroke
/// (e.g. `"j/k"`, `"Esc"`, `"?"`); `label` is the action verb
/// (e.g. `"nav"`, `"back"`, `"help"`). Empty `label` means the entry
/// is shown as keys-only even at wide widths.
#[derive(Debug, Clone, Copy)]
pub struct Entry {
    pub key: &'static str,
    pub label: &'static str,
}

impl Entry {
    pub const fn new(key: &'static str, label: &'static str) -> Self {
        Self { key, label }
    }
}

/// Render a footer for the given area. `entries` is in priority
/// order — entries are dropped from the *end* of the slice if the
/// total render would exceed `area.width`. At very narrow widths,
/// labels are dropped before entries (keys-only mode).
pub fn render(frame: &mut Frame<'_>, area: Rect, entries: &[Entry]) {
    let line = build_line(entries, area.width);
    frame.render_widget(Paragraph::new(line), area);
}

/// Pure render function — exposed for unit tests so we can assert
/// what's visible at each width without spinning up a TestBackend.
pub fn build_line(entries: &[Entry], width: u16) -> Line<'static> {
    let key_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().add_modifier(Modifier::DIM);
    let sep_style = Style::default().add_modifier(Modifier::DIM);

    // First pass: try with labels, dropping entries from the right
    // until everything fits.
    if let Some(spans) = pack(
        entries,
        width as usize,
        true,
        key_style,
        label_style,
        sep_style,
    ) {
        return Line::from(spans);
    }
    // Second pass: keys-only, dropping entries from the right.
    if let Some(spans) = pack(
        entries,
        width as usize,
        false,
        key_style,
        label_style,
        sep_style,
    ) {
        return Line::from(spans);
    }
    // Third pass: emit just the very first key, even if it overflows.
    // This is unreachable in practice (width <= 0 is the only path
    // here) but better than rendering nothing.
    let first = entries.first().map(|e| e.key).unwrap_or("");
    Line::from(Span::styled(format!(" {first} "), key_style))
}

fn pack(
    entries: &[Entry],
    budget: usize,
    with_labels: bool,
    key_style: Style,
    label_style: Style,
    sep_style: Style,
) -> Option<Vec<Span<'static>>> {
    if budget == 0 || entries.is_empty() {
        return None;
    }
    // Reserve 2 cells for leading + trailing whitespace padding so
    // the footer doesn't touch screen edges.
    let inner = budget.saturating_sub(2);

    // Try every prefix length from full down to 1, keep the longest
    // that fits. This is O(n) per call — fine for a 6–8 entry footer.
    for take in (1..=entries.len()).rev() {
        let slice = &entries[..take];
        let cost = render_cost(slice, with_labels);
        if cost <= inner {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(slice.len() * 4 + 2);
            spans.push(Span::raw(" "));
            for (i, e) in slice.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" · ", sep_style));
                }
                spans.push(Span::styled(e.key.to_string(), key_style));
                if with_labels && !e.label.is_empty() {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(e.label.to_string(), label_style));
                }
            }
            spans.push(Span::raw(" "));
            return Some(spans);
        }
    }
    None
}

/// Cell width that `slice` would consume in the chosen mode. Mirrors
/// the actual span layout above so `pack`'s budget check is honest.
fn render_cost(slice: &[Entry], with_labels: bool) -> usize {
    let mut cost = 0usize;
    for (i, e) in slice.iter().enumerate() {
        if i > 0 {
            cost += 3; // " · "
        }
        cost += e.key.chars().count();
        if with_labels && !e.label.is_empty() {
            cost += 1 + e.label.chars().count(); // space + label
        }
    }
    cost
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(entries: &[Entry], width: u16) -> String {
        // Flatten the rendered Line back to a plain string so tests
        // can assert on visible content without inspecting Spans.
        build_line(entries, width)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect::<String>()
    }

    fn sample() -> Vec<Entry> {
        vec![
            Entry::new("j/k", "nav"),
            Entry::new("Enter", "open"),
            Entry::new("d", "delete"),
            Entry::new("?", "help"),
            Entry::new("q", "quit"),
        ]
    }

    #[test]
    fn wide_shows_all_entries_with_labels() {
        let s = keys(&sample(), 80);
        assert!(s.contains("j/k"));
        assert!(s.contains("nav"));
        assert!(s.contains("Enter"));
        assert!(s.contains("open"));
        assert!(s.contains("d"));
        assert!(s.contains("delete"));
        assert!(s.contains("? help"));
        assert!(s.contains("q quit"));
    }

    #[test]
    fn narrow_keeps_quit_visible_by_dropping_least_important_first() {
        // 'q quit' is the LAST entry, so 'q' should never be dropped
        // before 'j/k'. Entries are dropped from the right; the
        // caller must order with most-important LAST? Wait — our
        // contract drops from the right (end of slice). To keep
        // quit visible at every width, the caller orders quit FIRST.
        // Let's verify: with quit-first ordering at narrow width.
        let entries = vec![
            Entry::new("q", "quit"),
            Entry::new("?", "help"),
            Entry::new("Enter", "open"),
            Entry::new("j/k", "nav"),
        ];
        let s = keys(&entries, 14);
        assert!(s.contains("q"), "quit must survive narrow width: {s:?}");
    }

    #[test]
    fn keys_only_mode_when_labels_dont_fit() {
        let entries = vec![
            Entry::new("q", "quit-the-application-now"),
            Entry::new("?", "show-the-help"),
        ];
        // Width too tight for the labels but enough for both keys.
        let s = keys(&entries, 10);
        assert!(s.contains("q"));
        assert!(s.contains("?"));
        // Labels should be absent.
        assert!(!s.contains("quit-the-application-now"));
        assert!(!s.contains("show-the-help"));
    }

    #[test]
    fn empty_entries_renders_empty_line() {
        let s = keys(&[], 80);
        assert!(s.trim().is_empty(), "expected empty footer, got: {s:?}");
    }
}
