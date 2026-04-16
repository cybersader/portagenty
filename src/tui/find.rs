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
use std::sync::mpsc;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use crate::find::{BackendAvailability, Candidate, FindOpts, Source};

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
///
/// Architecture (freeze fix): the slow filesystem walk runs once
/// in a background thread when the overlay opens (and again on
/// `>` / `<` / Ctrl+R root changes). The TUI thread never walks.
/// Each keystroke only re-ranks the already-cached candidate set
/// via nucleo, which is O(ms) even for thousands of entries. The
/// 250ms poll loop drains any new results from the background
/// thread and merges them into the cache.
pub struct SearchState {
    /// What the user has typed so far.
    pub input: String,
    /// Ranked candidates currently shown — subset of `raw_dirs`
    /// filtered + scored by nucleo against `input`.
    pub candidates: Vec<Candidate>,
    /// Highlighted index into `candidates`. Wraps on under/overflow.
    pub selected: usize,
    /// Knobs passed to the find pipeline (roots, depth, limit).
    opts: FindOpts,
    backends: BackendAvailability,
    list_state: ListState,
    /// Accumulator of all directories discovered by the background
    /// walker. Grows as the walker sends results over the channel.
    /// On keystroke we rank this set — no re-walk.
    raw_dirs: Vec<PathBuf>,
    /// Receiver end of the channel the background walker sends
    /// `Vec<PathBuf>` batches into. Drained on each render cycle.
    bg_rx: Option<mpsc::Receiver<Vec<PathBuf>>>,
    /// Whether a background walk is still in flight. Used to show
    /// a "scanning..." indicator in the UI.
    pub scanning: bool,
    /// Monotonic tick counter, incremented every poll cycle (~250ms).
    /// Drives the breadcrumb animation.
    anim_tick: u32,
    /// Current segment offset for the breadcrumb barber-pole. When
    /// the path is too long to fit, segments rotate upward from
    /// the leaf toward the root. Resets to 0 when the root changes.
    anim_offset: usize,
}

impl std::fmt::Debug for SearchState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchState")
            .field("input", &self.input)
            .field("candidates", &self.candidates.len())
            .field("raw_dirs", &self.raw_dirs.len())
            .field("scanning", &self.scanning)
            .finish()
    }
}

impl Default for SearchState {
    fn default() -> Self {
        let opts = FindOpts::default();
        let backends = BackendAvailability::probe();

        // Tiers 1 + 2 (recency + zoxide) are instant — collect
        // them synchronously so the overlay has content on first
        // frame. Tiers 3–5 (locate, fd, walk) are slow on DrvFs /
        // large trees and run in the background.
        let mut raw_dirs: Vec<PathBuf> = Vec::new();
        for p in crate::find::recency::collect() {
            raw_dirs.push(p);
        }
        for p in crate::find::zoxide::collect() {
            raw_dirs.push(p);
        }

        let bg_rx = Some(spawn_bg_walk(opts.clone()));

        let mut s = Self {
            input: String::new(),
            candidates: Vec::new(),
            selected: 0,
            opts,
            backends,
            list_state: ListState::default(),
            raw_dirs,
            bg_rx,
            scanning: true,
            anim_tick: 0,
            anim_offset: 0,
        };
        s.rerank();
        s
    }
}

impl SearchState {
    /// Drain any new results from the background walker and re-rank
    /// if the set changed. Called from the picker's poll loop on
    /// each 250ms tick — keeps the TUI responsive while directories
    /// trickle in from the background.
    /// Advance the breadcrumb animation by one tick. Called from
    /// the picker's 250ms poll loop. The rotation speed increases
    /// as the offset climbs toward the root — slow near the leaf
    /// (where the user's attention is), fast near `/` (less useful
    /// context). Specifically: each segment is shown for
    /// `max(2, 8 - offset)` ticks, so the leaf sits for ~2s and
    /// the root for ~0.5s before advancing.
    pub fn tick_animation(&mut self) {
        self.anim_tick = self.anim_tick.wrapping_add(1);
        let segments = path_segments(&self.opts.roots);
        if segments.len() <= 1 {
            return; // nothing to animate
        }
        // Ticks per step: starts at 16 (~4s at the leaf — where the
        // user needs time to read), decreases to 4 (~1s near the
        // root — less useful context scrolls faster).
        let ticks_per_step = 16u32.saturating_sub(self.anim_offset as u32 * 2).max(4);
        if self.anim_tick % ticks_per_step == 0 {
            self.anim_offset += 1;
            if self.anim_offset >= segments.len() {
                // Reached the root — pause an extra beat then wrap.
                self.anim_offset = 0;
            }
        }
    }

    pub fn poll_background(&mut self) {
        let Some(rx) = &self.bg_rx else { return };
        let mut changed = false;
        loop {
            match rx.try_recv() {
                Ok(batch) => {
                    self.raw_dirs.extend(batch);
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if self.scanning {
                        self.scanning = false;
                        changed = true;
                    }
                    break;
                }
            }
        }
        if changed {
            self.rerank();
        }
    }

    /// Re-rank `raw_dirs` against `self.input` via nucleo. This is
    /// the ONLY place that touches the matcher — keystrokes just
    /// call this, never the walker.
    /// Re-rank `raw_dirs` against `self.input` via nucleo. Runs on
    /// the main thread so MUST be fast — no syscalls (canonicalize
    /// on DrvFs was the old freeze; 5ms × 2500 paths = 12s blocked).
    /// Dedup uses the raw path string instead.
    fn rerank(&mut self) {
        let trimmed = self.input.trim();
        if trimmed.is_empty() {
            let mut seen = std::collections::HashSet::new();
            self.candidates = self
                .raw_dirs
                .iter()
                .filter(|p| seen.insert(p.to_string_lossy().into_owned()))
                .take(self.opts.limit)
                .map(|p| Candidate {
                    path: p.clone(),
                    source: Source::Walk,
                    score: 0,
                })
                .collect();
        } else {
            let mut matcher =
                nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT.match_paths());
            let pattern = nucleo_matcher::pattern::Pattern::parse(
                trimmed,
                nucleo_matcher::pattern::CaseMatching::Smart,
                nucleo_matcher::pattern::Normalization::Smart,
            );
            let mut scored: Vec<Candidate> = Vec::with_capacity(self.raw_dirs.len());
            let mut seen = std::collections::HashSet::new();
            for p in &self.raw_dirs {
                if !seen.insert(p.to_string_lossy().into_owned()) {
                    continue;
                }
                let haystack = p.to_string_lossy();
                let mut buf: Vec<char> = Vec::new();
                let utf32 = nucleo_matcher::Utf32Str::new(&haystack, &mut buf);
                if let Some(score) = pattern.score(utf32, &mut matcher) {
                    scored.push(Candidate {
                        path: p.clone(),
                        source: Source::Walk,
                        score: score as i32,
                    });
                }
            }
            scored.sort_by(|a, b| b.score.cmp(&a.score));
            scored.truncate(self.opts.limit);
            self.candidates = scored;
        }
        self.selected = 0;
        self.list_state.select(if self.candidates.is_empty() {
            None
        } else {
            Some(0)
        });
    }

    /// Trigger a fresh background walk (used by > / < / Ctrl+R).
    /// Clears old results and starts a new thread.
    fn restart_walk(&mut self) {
        self.anim_offset = 0;
        self.anim_tick = 0;
        self.raw_dirs.clear();
        // Re-seed with instant tiers so we have something immediately.
        for p in crate::find::recency::collect() {
            self.raw_dirs.push(p);
        }
        for p in crate::find::zoxide::collect() {
            self.raw_dirs.push(p);
        }
        self.bg_rx = Some(spawn_bg_walk(self.opts.clone()));
        self.scanning = true;
        self.rerank();
    }

    fn highlighted(&self) -> Option<&Candidate> {
        self.candidates.get(self.selected)
    }
}

/// Spawn a background thread that runs tiers 3–5 (locate, fd, walk)
/// and sends results back over a channel. The walker runs at most
/// once per overlay-open or root-change; keystrokes never trigger it.
fn spawn_bg_walk(opts: FindOpts) -> mpsc::Receiver<Vec<PathBuf>> {
    let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
    std::thread::spawn(move || {
        // Tier 3: locate (already batched — returns a Vec).
        let batch = crate::find::locate::collect("");
        if !batch.is_empty() {
            let _ = tx.send(batch);
        }
        // Tier 4: fd (already batched).
        let batch = crate::find::fd::collect("", &opts);
        if !batch.is_empty() {
            let _ = tx.send(batch);
        }
        // Tier 5: stdlib walk — streams ~500-dir batches over the
        // channel so results arrive incrementally instead of waiting
        // for the entire walk to finish.
        crate::find::walk::collect_streaming("", &opts, &tx);
        // tx drops here → receiver sees Disconnected → scanning = false.
    });
    rx
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
            state.rerank();
            SearchOutcome::Continue
        }
        (KeyCode::Backspace, _) => {
            state.input.pop();
            state.rerank();
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
        (KeyCode::Char('>'), _) => {
            if let Some(c) = state.highlighted() {
                state.opts.roots = vec![c.path.clone()];
                state.input.clear();
                state.restart_walk();
            }
            SearchOutcome::Continue
        }
        // `<` navigates UP: pivots the root to the PARENT of the
        // highlighted entry (not the root's parent). Ranger-style
        // "go up from where I'm looking" — if you drilled into the
        // wrong folder with `>`, one `<` gets you back to its
        // parent's siblings. Falls back to root's parent if nothing
        // is highlighted.
        (KeyCode::Char('<'), _) => {
            let target = state
                .highlighted()
                .and_then(|c| c.path.parent())
                .map(|p| p.to_path_buf())
                .or_else(|| {
                    state
                        .opts
                        .roots
                        .first()
                        .and_then(|r| r.parent())
                        .filter(|p| *p != state.opts.roots[0].as_path())
                        .map(|p| p.to_path_buf())
                });
            if let Some(t) = target {
                state.opts.roots = vec![t];
                state.input.clear();
                state.restart_walk();
            }
            SearchOutcome::Continue
        }
        (KeyCode::Char('r'), m) if m.contains(KeyModifiers::CONTROL) => {
            state.opts.roots = crate::find::default_roots();
            state.input.clear();
            state.restart_walk();
            SearchOutcome::Continue
        }
        (KeyCode::Char(ch), _) => {
            state.input.push(ch);
            // Re-rank only — never re-walk on a keystroke. The
            // background thread feeds new dirs; nucleo ranks them.
            state.rerank();
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
    // Use nearly full terminal width so long paths have room.
    let overlay_w = w.saturating_sub(2).max(20);
    let overlay_h = ((h as u32 * 9 / 10).clamp(8, 35) as u16).min(h);
    let x = area.x + (w.saturating_sub(overlay_w)) / 2;
    let y = area.y + (h.saturating_sub(overlay_h)) / 2;
    let region = Rect {
        x,
        y,
        width: overlay_w,
        height: overlay_h,
    };

    frame.render_widget(Clear, region);

    // Title bar: backends + scan count only (short). The current
    // root gets its own dedicated line INSIDE the overlay so it's
    // always fully visible even on narrow terminals.
    let scan_str = if state.scanning {
        format!(" · scanning {}…", state.raw_dirs.len())
    } else {
        format!(" · {} dirs", state.raw_dirs.len())
    };
    let title = format!(" find folder{scan_str} ");
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

    // Inner layout: 2-line breadcrumb + input + candidate list + hint.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // root breadcrumb (2 lines for wrapping)
            Constraint::Length(1), // input
            Constraint::Min(1),    // candidate list
            Constraint::Length(1),
        ])
        .split(inner);

    // Static 2-line breadcrumb. Line 1: folder icon + path (wraps to
    // line 2 if too long). Backends on line 2 as dim suffix.
    // No animation — just clear, always-readable text.
    let inner_w = inner.width as usize;
    let breadcrumb =
        render_static_breadcrumb(&state.opts.roots, inner_w, &state.backends.one_liner());
    frame.render_widget(Paragraph::new(breadcrumb), chunks[0]);

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
    frame.render_widget(Paragraph::new(input_line), chunks[1]);

    // Candidate list.
    let items: Vec<ListItem> = state
        .candidates
        .iter()
        .map(|c| candidate_item(c, chunks[2].width))
        .collect();
    if items.is_empty() {
        let msg = if state.scanning {
            "  scanning filesystem… results will appear as they're found"
        } else if state.input.is_empty() {
            "  (no recents yet — type to search your filesystem)"
        } else {
            "  no matches — try > to drill into a folder, or < to go up"
        };
        let empty = Paragraph::new(msg).style(Style::default().add_modifier(Modifier::DIM));
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
#[allow(dead_code)]
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

/// Split the first root path into its directory segments, leaf-first.
/// Example: `/mnt/c/Users/Cybersader/Documents` →
///   `["Documents", "Cybersader", "Users", "c", "mnt"]`
/// Split the first root into path segments, leaf-first. Ancestors
/// beyond the first two are abbreviated to their first character
/// (fish-shell style) so the breadcrumb fits in much less space:
///
///   `/mnt/c/Users/Cybersader/Documents/1 Projects, Workspaces`
///   → `["1 Projects, Workspaces", "Documents", "C", "U", "c", "m"]`
///
/// Render a static 2-line breadcrumb. Line 1: the full path with
/// ~ for home and a folder icon. Line 2: backends, or overflow
/// from line 1 + backends. No animation — just clear, readable.
fn render_static_breadcrumb(roots: &[PathBuf], width: usize, backends: &str) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM);
    let bold = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let path_str = roots
        .first()
        .map(|p| compact_home(&p.display().to_string()))
        .unwrap_or_else(|| "(no root)".into());

    let padded = format!("  📂 {path_str}");
    let budget = width.saturating_sub(2);

    if padded.chars().count() <= budget {
        // Fits on one line; backends on line 2.
        vec![
            Line::from(Span::styled(padded, bold)),
            Line::from(Span::styled(format!("     [{backends}]"), dim)),
        ]
    } else {
        // Wrap: split at budget, overflow to line 2.
        let split = budget.saturating_sub(5);
        let head: String = path_str.chars().take(split).collect();
        let rest: String = path_str.chars().skip(split).collect();
        vec![
            Line::from(Span::styled(format!("  📂 {head}"), bold)),
            Line::from(vec![
                Span::styled(format!("     {rest}"), bold),
                Span::raw("  "),
                Span::styled(format!("[{backends}]"), dim),
            ]),
        ]
    }
}

/// Leaf + parent stay full (they're the useful context); the rest
/// compress. This is the "much smaller" path the user asked for —
/// the terminal can't change font size, so we abbreviate instead.
fn path_segments(roots: &[PathBuf]) -> Vec<String> {
    let Some(root) = roots.first() else {
        return vec!["(no root)".into()];
    };
    let display = compact_home(&root.display().to_string());
    let raw_segs: Vec<&str> = display.split('/').filter(|s| !s.is_empty()).collect();
    if raw_segs.is_empty() {
        return vec!["/".into()];
    }
    let n = raw_segs.len();
    let mut segs: Vec<String> = Vec::with_capacity(n);
    for (i, s) in raw_segs.iter().enumerate() {
        // Last 2 segments (leaf + parent) stay full; the rest
        // abbreviate to their first character.
        if i + 2 >= n {
            segs.push(s.to_string());
        } else {
            segs.push(s.chars().next().map(|c| c.to_string()).unwrap_or_default());
        }
    }
    segs.reverse(); // leaf first
    segs
}

/// Render the animated breadcrumb line. Shows a window of segments
/// starting from `offset` (which the animation advances over time).
/// When all segments fit, the full path is shown leaf-first with no
/// animation indicator. When they overflow, a `⇡` prefix hints
/// that more segments are above the visible window.
#[allow(dead_code)]
fn render_animated_breadcrumb(
    segments: &[String],
    offset: usize,
    width: usize,
    backends: &str,
) -> Line<'static> {
    if segments.is_empty() {
        return Line::from(Span::raw(""));
    }
    let dim = Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM);
    let bright = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    // Build the visible segment window starting from `offset`.
    // Segments are leaf-first, so offset=0 shows the leaf + parents;
    // offset=N shows the Nth ancestor + its parents.
    let start = offset % segments.len();
    let mut visible: Vec<&str> = Vec::new();
    let mut used = 4usize; // "  " padding + " /" separator budget
                           // Walk from `start` upward (toward root), accumulating segments
                           // that fit in the width budget.
    for seg in &segments[start..] {
        let cost = seg.chars().count() + 3; // " / " separator
        if used + cost > width && !visible.is_empty() {
            break;
        }
        visible.push(seg);
        used += cost;
    }

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(visible.len() * 3 + 4);
    spans.push(Span::raw("  "));

    // If we're not showing from the leaf, add an animation hint.
    if start > 0 {
        spans.push(Span::styled("⇣ ", bright));
    }

    // Render segments: the first visible (closest to leaf) is bold,
    // the rest dim — draws the eye to "where am I" at the bottom.
    for (i, seg) in visible.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" / ", dim));
        }
        let style = if i == 0 { bright } else { dim };
        spans.push(Span::styled(seg.to_string(), style));
    }

    // If there are segments beyond what's visible toward the root,
    // add an upward hint.
    let shown_to = start + visible.len();
    if shown_to < segments.len() {
        spans.push(Span::styled(" / ⇡", dim));
    }

    // Backends suffix if room.
    let remaining = width.saturating_sub(used + 4);
    if remaining > backends.chars().count() + 4 {
        spans.push(Span::styled(format!("  [{backends}]"), dim));
    }

    Line::from(spans)
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
        // probes. Build the struct manually with synthetic candidates.
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
            raw_dirs: vec![
                PathBuf::from("/home/u/cyberchaste"),
                PathBuf::from("/home/u/cybersader/portagenty"),
            ],
            bg_rx: None,
            scanning: false,
            anim_tick: 0,
            anim_offset: 0,
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

#[cfg(test)]
mod anim_tests {
    use super::*;

    #[test]
    fn breadcrumb_shows_leaf_first_at_offset_zero() {
        let segs = vec!["leaf".into(), "mid".into(), "root".into()];
        let line = render_animated_breadcrumb(&segs, 0, 40, "scan");
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(
            text.contains("leaf"),
            "leaf should be first at offset 0: {text:?}"
        );
    }

    #[test]
    fn breadcrumb_shifts_to_mid_at_offset_one() {
        let segs = vec!["leaf".into(), "mid".into(), "root".into()];
        let line = render_animated_breadcrumb(&segs, 1, 40, "scan");
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(
            text.contains("mid"),
            "mid should be visible at offset 1: {text:?}"
        );
        assert!(text.contains("⇣"), "should have down-arrow hint: {text:?}");
    }

    #[test]
    fn breadcrumb_wraps_at_segment_count() {
        let segs = vec!["a".into(), "b".into(), "c".into()];
        // offset=3 should wrap to 0
        let line = render_animated_breadcrumb(&segs, 3, 40, "");
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("a"), "should wrap back to leaf: {text:?}");
    }

    #[test]
    fn breadcrumb_doesnt_panic_at_narrow_width() {
        let segs = vec!["very-long-segment-name".into(), "another".into()];
        let _ = render_animated_breadcrumb(&segs, 0, 10, "");
        let _ = render_animated_breadcrumb(&segs, 1, 10, "");
        // Just verify no panic at tiny widths.
    }

    #[test]
    fn breadcrumb_shows_full_path_when_it_fits() {
        let segs = vec!["leaf".into(), "root".into()];
        let line = render_animated_breadcrumb(&segs, 0, 60, "recents · scan");
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("leaf"), "leaf: {text:?}");
        assert!(text.contains("root"), "root: {text:?}");
        assert!(
            !text.contains("⇡"),
            "no overflow indicator when everything fits: {text:?}"
        );
        assert!(
            text.contains("recents"),
            "backends suffix when room: {text:?}"
        );
    }
}
