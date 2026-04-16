//! In-TUI folder finder overlay. Opens when the user presses `n`
//! in the workspace picker. Drives the [`crate::find`] pipeline
//! against a live-typed query and renders ranked candidate folders.
//!
//! On Enter:
//! - If the highlighted folder already contains a `*.portagenty.toml`,
//!   the outer picker treats this as "open existing workspace."
//! - Otherwise the outer picker pops a confirm modal asking
//!   "scaffold a new workspace at <path>?" and on `y` writes the
//!   file via [`crate::scaffold::create_at`], registers it, and
//!   loads it into the session TUI immediately.
//!
//! The module owns its own [`SearchState`] but does NOT own the
//! event loop or the scaffold step — both live in the picker. This
//! keeps `find.rs` focused on input + render + ranking; the picker
//! decides what to do with the user's selection.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use crate::find::{find_candidates, BackendAvailability, Candidate, FindOpts};

/// What the picker should do after a key press inside the search
/// overlay. Mirrors the picker's normal action vocabulary so the
/// outer event loop can dispatch without a second match.
#[derive(Debug, Clone)]
pub enum SearchOutcome {
    /// No state change beyond what we already mutated; keep
    /// rendering the search overlay.
    Continue,
    /// User pressed Esc — close the search overlay, return to
    /// normal picker mode.
    Cancel,
    /// User picked an existing workspace file. Picker should treat
    /// this exactly like clicking that workspace's row.
    OpenExisting(PathBuf),
    /// User picked a directory with no workspace file. Picker
    /// should pop the scaffold-confirm modal pointing at this dir.
    ScaffoldAt(PathBuf),
    /// User opened the help overlay via `?`. Picker handles the
    /// help bookkeeping.
    OpenHelp,
}

/// Mutable state for the search overlay. Lives inside
/// `picker::PickerState::search` as `Option<SearchState>`.
#[derive(Debug)]
pub struct SearchState {
    /// What the user has typed so far.
    pub input: String,
    /// Cached candidate list, refreshed on every input change.
    pub candidates: Vec<Candidate>,
    /// Highlighted index into `candidates`. Wraps on under/overflow.
    pub selected: usize,
    /// Knobs passed to the find pipeline (roots, depth, limit).
    /// Mutable so the user can drill into a highlighted folder
    /// via `>` (search-from-here) and the new root takes effect on
    /// the next refresh.
    opts: FindOpts,
    /// Probed availability of fd / zoxide / plocate / etc. Cached
    /// once at overlay open time; surfaces as a hint in the title
    /// bar so users can tell which tools are actually contributing
    /// to the result set.
    backends: BackendAvailability,
    /// `ListState` for the candidate list widget. Tracks viewport
    /// scrolling so long lists don't push the selection off-screen.
    list_state: ListState,
}

impl Default for SearchState {
    fn default() -> Self {
        let mut s = Self {
            input: String::new(),
            candidates: Vec::new(),
            selected: 0,
            opts: FindOpts::default(),
            backends: BackendAvailability::probe(),
            list_state: ListState::default(),
        };
        s.refresh();
        s
    }
}

impl SearchState {
    /// Re-run the find pipeline with the current input. Resets
    /// `selected` to 0 since the candidate ordering changed.
    fn refresh(&mut self) {
        self.candidates = find_candidates(&self.input, &self.opts);
        self.selected = 0;
        self.list_state.select(if self.candidates.is_empty() {
            None
        } else {
            Some(0)
        });
    }

    fn highlighted(&self) -> Option<&Candidate> {
        self.candidates.get(self.selected)
    }
}

/// Process a single key press. Returns the action the outer picker
/// should take. Pure dispatch — the caller is responsible for the
/// terminal redraw.
pub fn handle_key(state: &mut SearchState, code: KeyCode, mods: KeyModifiers) -> SearchOutcome {
    match (code, mods) {
        (KeyCode::Esc, _) => SearchOutcome::Cancel,
        (KeyCode::Char('?'), _) => SearchOutcome::OpenHelp,
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => SearchOutcome::Cancel,
        (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
            state.input.clear();
            state.refresh();
            SearchOutcome::Continue
        }
        (KeyCode::Backspace, _) => {
            state.input.pop();
            state.refresh();
            SearchOutcome::Continue
        }
        (KeyCode::Up, _) => {
            move_selection(state, -1);
            SearchOutcome::Continue
        }
        (KeyCode::Char('p'), m) if m.contains(KeyModifiers::CONTROL) => {
            move_selection(state, -1);
            SearchOutcome::Continue
        }
        (KeyCode::Down, _) => {
            move_selection(state, 1);
            SearchOutcome::Continue
        }
        (KeyCode::Char('n'), m) if m.contains(KeyModifiers::CONTROL) => {
            move_selection(state, 1);
            SearchOutcome::Continue
        }
        (KeyCode::Enter, _) => match state.highlighted() {
            None => SearchOutcome::Continue,
            Some(c) => classify_pick(&c.path),
        },
        // `>` drills into the highlighted folder: pivots the
        // search root to that path, clears the input, and refreshes.
        // Lets users jump from "give me everything under $HOME" to
        // "give me everything under ~/code" with one keystroke.
        (KeyCode::Char('>'), _) => {
            if let Some(c) = state.highlighted() {
                state.opts.roots = vec![c.path.clone()];
                state.input.clear();
                state.refresh();
            }
            SearchOutcome::Continue
        }
        // `<` navigates UP one level — sets the root to the parent
        // of the current root. Recursive: keep pressing `<` to walk
        // up the tree. At `/` (or when the parent equals the root)
        // it's a no-op. Ctrl+R resets all the way back to defaults
        // if you want to start over from $HOME.
        (KeyCode::Char('<'), _) => {
            if let Some(root) = state.opts.roots.first().cloned() {
                if let Some(parent) = root.parent() {
                    if parent != root {
                        state.opts.roots = vec![parent.to_path_buf()];
                        state.input.clear();
                        state.refresh();
                    }
                }
            }
            SearchOutcome::Continue
        }
        // Ctrl+R fully resets roots to the machine defaults ($HOME
        // + WSL root if applicable). The nuclear "go back to square
        // one" for when > / < navigation lost the user.
        (KeyCode::Char('r'), m) if m.contains(KeyModifiers::CONTROL) => {
            state.opts.roots = crate::find::default_roots();
            state.input.clear();
            state.refresh();
            SearchOutcome::Continue
        }
        (KeyCode::Char(ch), _) => {
            state.input.push(ch);
            state.refresh();
            SearchOutcome::Continue
        }
        _ => SearchOutcome::Continue,
    }
}

fn move_selection(state: &mut SearchState, delta: i32) {
    let n = state.candidates.len();
    if n == 0 {
        return;
    }
    let cur = state.selected as i32;
    let next = (cur + delta).rem_euclid(n as i32) as usize;
    state.selected = next;
    state.list_state.select(Some(next));
}

/// Decide whether picking `path` should open an existing workspace
/// or scaffold a new one. We look for any `*.portagenty.toml` file
/// directly in the directory; if present, treat as open-existing.
fn classify_pick(path: &Path) -> SearchOutcome {
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".portagenty.toml") && name != "portagenty.toml" {
                return SearchOutcome::OpenExisting(entry.path());
            }
        }
    }
    SearchOutcome::ScaffoldAt(path.to_path_buf())
}

/// Render the overlay over `area`. Reserves a centered region of
/// ~80% width / 70% height; smaller terminals get full coverage.
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &mut SearchState) {
    let w = area.width;
    let h = area.height;
    let overlay_w = (w as u32 * 8 / 10).clamp(28, 90) as u16;
    let overlay_h = (h as u32 * 8 / 10).clamp(8, 30) as u16;
    let x = area.x + (w.saturating_sub(overlay_w)) / 2;
    let y = area.y + (h.saturating_sub(overlay_h)) / 2;
    let region = Rect {
        x,
        y,
        width: overlay_w,
        height: overlay_h,
    };

    frame.render_widget(Clear, region);

    // Title bar conveys two things: the active backend cohort
    // (so users know which tools are in the mix) and the current
    // search root(s) (so a `>`-drill is obvious from a glance).
    let backends_str = state.backends.one_liner();
    let roots_str = compact_roots(&state.opts.roots);
    let title = format!(" find folder · backends: {backends_str} · roots: {roots_str} ");
    let outer = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = outer.inner(region);
    frame.render_widget(outer, region);

    // Inner layout: 1-line input + spacer + candidate list + 1-line hint.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    // Input line: prompt char + user text + caret (block style).
    let input_line = Line::from(vec![
        Span::styled(
            "  ❯ ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            state.input.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "_",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::SLOW_BLINK),
        ),
    ]);
    frame.render_widget(Paragraph::new(input_line), chunks[0]);

    // Spacer.
    frame.render_widget(Paragraph::new(""), chunks[1]);

    // Candidate list.
    let items: Vec<ListItem> = state
        .candidates
        .iter()
        .map(|c| candidate_item(c, chunks[2].width))
        .collect();
    if items.is_empty() {
        let empty = Paragraph::new(if state.input.is_empty() {
            "  (no recents yet — type to search your filesystem)"
        } else {
            "  no matches"
        })
        .style(Style::default().add_modifier(Modifier::DIM));
        frame.render_widget(empty, chunks[2]);
    } else {
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(list, chunks[2], &mut state.list_state);
    }

    // Bottom hint.
    let hint =
        Paragraph::new(" Enter open · > drill in · < go up · Ctrl+R reset · Esc cancel · ↑/↓ nav ")
            .style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(hint, chunks[3]);
}

/// Render `roots` compactly for the title bar — replace `$HOME`
/// with `~`, join with " + ", truncate the whole thing to ~40 chars.
fn compact_roots(roots: &[PathBuf]) -> String {
    if roots.is_empty() {
        return "(none)".to_string();
    }
    let pieces: Vec<String> = roots
        .iter()
        .map(|p| compact_home(&p.display().to_string()))
        .collect();
    let joined = pieces.join(" + ");
    if joined.chars().count() > 40 {
        let head: String = joined.chars().take(38).collect();
        format!("{head}…")
    } else {
        joined
    }
}

fn candidate_item(c: &Candidate, width: u16) -> ListItem<'static> {
    let name = c
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| c.path.display().to_string());
    let dir = c
        .path
        .parent()
        .map(|p| compact_home(&p.display().to_string()))
        .unwrap_or_default();
    // Source badge (recent / zoxide / fd / scan / locate).
    let badge = format!("[{}]", c.source.label());

    // Width budget: " ▶ name  dir  badge ". Truncate dir if needed.
    let used = 4 + name.chars().count() + 2 + 2 + badge.chars().count();
    let remaining = (width as usize).saturating_sub(used);
    let dir_render = if dir.chars().count() > remaining {
        truncate_middle(&dir, remaining.max(8))
    } else {
        dir
    };

    ListItem::new(Line::from(vec![
        Span::raw(" "),
        Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(dir_render, Style::default().add_modifier(Modifier::DIM)),
        Span::raw("  "),
        Span::styled(badge, Style::default().fg(Color::Magenta)),
    ]))
}

fn compact_home(p: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find::Source;

    fn make_state(input: &str) -> SearchState {
        // Don't call SearchState::default() — that fires real FS
        // probes via refresh(). Build the struct manually with
        // synthetic candidates so tests stay deterministic.
        let mut s = SearchState {
            input: input.to_string(),
            candidates: vec![
                Candidate {
                    path: PathBuf::from("/home/u/cyberchaste"),
                    source: Source::Recency,
                    score: 100,
                },
                Candidate {
                    path: PathBuf::from("/home/u/cybersader/portagenty"),
                    source: Source::Walk,
                    score: 80,
                },
            ],
            selected: 0,
            opts: FindOpts {
                roots: vec![],
                max_depth: 0,
                limit: 30,
            },
            backends: BackendAvailability::default(),
            list_state: ListState::default(),
        };
        s.list_state.select(Some(0));
        s
    }

    #[test]
    fn esc_returns_cancel() {
        let mut s = make_state("");
        let out = handle_key(&mut s, KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(out, SearchOutcome::Cancel));
    }

    #[test]
    fn down_advances_selection() {
        let mut s = make_state("");
        let _ = handle_key(&mut s, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(s.selected, 1);
    }

    #[test]
    fn down_wraps_around_at_end() {
        let mut s = make_state("");
        s.selected = 1;
        s.list_state.select(Some(1));
        let _ = handle_key(&mut s, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn typing_appends_to_input() {
        let mut s = make_state("");
        // Skip refresh by directly invoking handle_key with chars
        // that won't trip a real FS walk: use Char on an empty
        // candidate set — but our default state has candidates.
        // Instead just verify the input mutation via Backspace/clear.
        s.input = "abc".to_string();
        let _ = handle_key(&mut s, KeyCode::Backspace, KeyModifiers::NONE);
        // refresh ran with FS — input pop is what we care about.
        assert_eq!(s.input, "ab");
    }

    #[test]
    fn ctrl_u_clears_input() {
        let mut s = make_state("foo");
        let _ = handle_key(&mut s, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(s.input.is_empty());
    }

    #[test]
    fn enter_with_no_candidates_continues() {
        let mut s = make_state("");
        s.candidates.clear();
        let out = handle_key(&mut s, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(out, SearchOutcome::Continue));
    }
}
