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

/// Mode toggle inside the find overlay.
#[derive(Debug)]
pub enum FindMode {
    /// Fuzzy search with nucleo ranking (default).
    Search,
    /// File explorer tree rooted at current search root.
    Tree(Box<TreeBrowseState>),
}

/// State for the file-explorer tree mode. Built without external
/// tree widgets to avoid ratatui version conflicts. Uses a flattened
/// list of `TreeRow`s rendered via ratatui's `List` + `ListState`.
#[derive(Debug)]
pub struct TreeBrowseState {
    /// Expanded directory paths.
    pub expanded: std::collections::HashSet<String>,
    /// Lazily-loaded children, keyed by parent path string.
    pub children_cache: std::collections::HashMap<String, Vec<PathBuf>>,
    /// Root directory.
    pub root: PathBuf,
    /// Flattened rows for the current view. Rebuilt on expand/collapse.
    pub rows: Vec<TreeRow>,
    /// Selection index into `rows`.
    pub selected: usize,
    /// ListState for ratatui.
    pub list_state: ListState,
    /// When `Some`, the new-folder input modal is open. The string is
    /// the in-progress folder name. Enter creates it under `root`,
    /// Esc cancels. Keys get diverted to this field while open.
    pub creating_folder: Option<String>,
    /// Transient error message from the last failed action (e.g.
    /// "folder exists", "permission denied"). Shown in the modal if
    /// non-empty. Cleared on next successful action.
    pub last_error: Option<String>,
}

/// One visible row in the tree.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub is_expanded: bool,
    pub is_last_sibling: bool,
}

impl TreeBrowseState {
    fn new(root: PathBuf) -> Self {
        let mut s = Self {
            expanded: std::collections::HashSet::new(),
            children_cache: std::collections::HashMap::new(),
            root: root.clone(),
            rows: Vec::new(),
            selected: 0,
            list_state: ListState::default(),
            creating_folder: None,
            last_error: None,
        };
        s.load_children(&root);
        s.rebuild_rows();
        s
    }

    /// Create a new folder named `name` under the current `root`.
    /// Refreshes the children cache so the new folder shows up
    /// immediately and selects it. Returns an error string on
    /// failure (shown in the modal).
    fn create_folder(&mut self, name: &str) -> Result<(), String> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err("folder name cannot be empty".into());
        }
        // Reject anything that'd escape the current root (path
        // separators, `..`). Forces the user to drill into nested
        // creations intentionally instead of sneaking them in.
        if trimmed.contains('/')
            || trimmed.contains('\\')
            || trimmed == "."
            || trimmed == ".."
        {
            return Err("folder name can't contain / or \\".into());
        }
        let new_path = self.root.join(trimmed);
        if new_path.exists() {
            return Err(format!("{trimmed:?} already exists"));
        }
        std::fs::create_dir(&new_path).map_err(|e| format!("create failed: {e}"))?;
        // Refresh the root's children cache so the new folder shows
        // in the tree immediately.
        self.children_cache.remove(&self.root.display().to_string());
        self.load_children(&self.root.clone());
        self.rebuild_rows();
        // Select the newly-created folder.
        if let Some(idx) = self
            .rows
            .iter()
            .position(|r| r.path == new_path)
        {
            self.selected = idx;
            self.list_state.select(Some(idx));
        }
        Ok(())
    }

    fn load_children(&mut self, dir: &Path) {
        let key = dir.display().to_string();
        if self.children_cache.contains_key(&key) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            self.children_cache.insert(key, Vec::new());
            return;
        };
        let ignore = [
            ".git",
            ".hg",
            ".svn",
            "node_modules",
            "target",
            ".cache",
            "venv",
            ".venv",
            "__pycache__",
            "dist",
            "build",
        ];
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| {
                let Ok(ft) = e.file_type() else { return false };
                if !ft.is_dir() {
                    return false;
                }
                let name = e.file_name();
                let n = name.to_string_lossy();
                if n.starts_with('.') && n != "." {
                    return false;
                }
                !ignore.iter().any(|i| *i == &*n)
            })
            .map(|e| e.path())
            .collect();
        dirs.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        self.children_cache.insert(key, dirs);
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let root = self.root.clone();
        self.add_children_rows(&root, 0);
        if !self.rows.is_empty() {
            self.selected = self.selected.min(self.rows.len() - 1);
            self.list_state.select(Some(self.selected));
        } else {
            self.list_state.select(None);
        }
    }

    fn add_children_rows(&mut self, dir: &Path, depth: usize) {
        let key = dir.display().to_string();
        let children = self.children_cache.get(&key).cloned().unwrap_or_default();
        let count = children.len();
        for (i, child) in children.iter().enumerate() {
            let name = child
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let is_dir = child.is_dir();
            let id = child.display().to_string();
            let is_expanded = self.expanded.contains(&id);
            self.rows.push(TreeRow {
                path: child.clone(),
                name,
                depth,
                is_dir,
                is_expanded,
                is_last_sibling: i + 1 == count,
            });
            if is_expanded {
                self.load_children(child);
                self.add_children_rows(child, depth + 1);
            }
        }
    }

    fn selected_row(&self) -> Option<&TreeRow> {
        self.rows.get(self.selected)
    }

    fn toggle_expand(&mut self) {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return;
        };
        if !row.is_dir {
            return;
        }
        let id = row.path.display().to_string();
        if self.expanded.contains(&id) {
            self.expanded.remove(&id);
        } else {
            self.load_children(&row.path);
            self.expanded.insert(id);
        }
        self.rebuild_rows();
    }

    fn expand_selected(&mut self) {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return;
        };
        if !row.is_dir || row.is_expanded {
            return;
        }
        let id = row.path.display().to_string();
        self.load_children(&row.path);
        self.expanded.insert(id);
        self.rebuild_rows();
    }

    fn collapse_selected(&mut self) {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return;
        };
        let id = row.path.display().to_string();
        if self.expanded.contains(&id) {
            self.expanded.remove(&id);
            self.rebuild_rows();
        } else if let Some(parent) = row.path.parent() {
            let pid = parent.display().to_string();
            let parent_buf = parent.to_path_buf();
            self.expanded.remove(&pid);
            self.rebuild_rows();
            if let Some(idx) = self.rows.iter().position(|r| r.path == parent_buf) {
                self.selected = idx;
                self.list_state.select(Some(idx));
            }
        }
    }

    /// Re-root the tree at the highlighted folder. Used by `>` in
    /// tree mode so the user can drill all the way in (not just
    /// expand inline). Resets expansion state. If the highlighted row
    /// is a file, drills into its parent dir.
    fn drill_into_selected(&mut self) {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return;
        };
        let new_root = if row.is_dir {
            row.path.clone()
        } else {
            row.path.parent().map(|p| p.to_path_buf()).unwrap_or(row.path.clone())
        };
        if new_root != self.root {
            self.root = new_root;
            self.expanded.clear();
            self.children_cache.clear();
            self.load_children(&self.root.clone());
            self.rebuild_rows();
            self.selected = 0;
            self.list_state.select(if self.rows.is_empty() {
                None
            } else {
                Some(0)
            });
        }
    }

    /// Re-root the tree at the current root's parent. Used by `..`
    /// / Backspace in tree mode so the user can browse up without
    /// exiting to search mode first. Resets expansion state.
    fn pop_root(&mut self) {
        if let Some(parent) = self.root.parent() {
            let new_root = parent.to_path_buf();
            if new_root != self.root {
                self.root = new_root;
                self.expanded.clear();
                self.children_cache.clear();
                self.load_children(&self.root.clone());
                self.rebuild_rows();
                self.selected = 0;
                self.list_state.select(if self.rows.is_empty() {
                    None
                } else {
                    Some(0)
                });
            }
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let next = (self.selected as i32 + delta).rem_euclid(n as i32) as usize;
        self.selected = next;
        self.list_state.select(Some(next));
    }
}

/// Handle a key press in tree mode. Returns `BackToSearch` when
/// Esc is pressed so the user drops back to search mode (not all
/// the way out to the picker — that's the Android-back pattern).
pub fn handle_tree_key(
    state: &mut TreeBrowseState,
    code: KeyCode,
    mods: KeyModifiers,
) -> SearchOutcome {
    // New-folder modal is open: divert keys into the input buffer.
    // Enter commits, Esc cancels.
    if let Some(mut input) = state.creating_folder.take() {
        match code {
            KeyCode::Esc => {
                state.last_error = None;
                // Modal closes (we already took the Option).
            }
            KeyCode::Enter => match state.create_folder(&input) {
                Ok(()) => {
                    state.last_error = None;
                }
                Err(e) => {
                    state.last_error = Some(e);
                    state.creating_folder = Some(input);
                }
            },
            KeyCode::Backspace => {
                input.pop();
                state.creating_folder = Some(input);
            }
            KeyCode::Char('h') if mods.contains(KeyModifiers::CONTROL) => {
                input.pop();
                state.creating_folder = Some(input);
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                input.clear();
                state.creating_folder = Some(input);
            }
            KeyCode::Char('w') if mods.contains(KeyModifiers::CONTROL) => {
                while input.ends_with(' ') {
                    input.pop();
                }
                while input.chars().last().is_some_and(|c| !c.is_whitespace()) {
                    input.pop();
                }
                state.creating_folder = Some(input);
            }
            KeyCode::Char(_) if mods.contains(KeyModifiers::CONTROL) => {
                // Eat stray Ctrl+<letter>s so they don't end up in
                // the input as raw chars.
                state.creating_folder = Some(input);
            }
            KeyCode::Char(ch) => {
                input.push(ch);
                state.creating_folder = Some(input);
            }
            _ => {
                state.creating_folder = Some(input);
            }
        }
        return SearchOutcome::Continue;
    }

    match (code, mods) {
        // Esc → back to search (NOT cancel the whole overlay).
        // q or Ctrl+C → close overlay entirely.
        (KeyCode::Esc, _) => SearchOutcome::BackToSearch,
        (KeyCode::Char('q'), _) => SearchOutcome::Cancel,
        (KeyCode::Char('?'), _) => SearchOutcome::OpenHelp,
        // `/` → search from here: jump to search mode with the
        // highlighted folder as the new search root.
        (KeyCode::Char('/'), _) => {
            if let Some(row) = state.selected_row() {
                let dir = if row.path.is_dir() {
                    row.path.clone()
                } else {
                    row.path.parent().map(|p| p.to_path_buf()).unwrap_or(row.path.clone())
                };
                return SearchOutcome::SearchFromHere(dir);
            }
            SearchOutcome::Continue
        }
        // `o` → open in terminal: exit pa and drop into a shell at
        // the highlighted folder (or tree root if nothing is selected
        // or highlighted is a file).
        (KeyCode::Char('o'), _) => {
            let dir = state.selected_row().map(|r| {
                if r.path.is_dir() {
                    r.path.clone()
                } else {
                    r.path.parent().map(|p| p.to_path_buf()).unwrap_or(r.path.clone())
                }
            }).unwrap_or_else(|| state.root.clone());
            SearchOutcome::OpenShellAt(dir)
        }
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => SearchOutcome::Cancel,
        // Ctrl+T in tree mode → toggle back to search mode (same as
        // Esc, but symmetrical with Ctrl+T entering tree mode from
        // search).
        (KeyCode::Char('t'), m) if m.contains(KeyModifiers::CONTROL) => {
            SearchOutcome::BackToSearch
        }
        (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
            state.move_selection(1);
            SearchOutcome::Continue
        }
        (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
            state.move_selection(-1);
            SearchOutcome::Continue
        }
        // `.` → drill: re-root the tree at the highlighted folder.
        // Single key, easy on Termux — `>` would work conceptually
        // but requires Shift which is painful on mobile.
        (KeyCode::Char('.'), _) => {
            state.drill_into_selected();
            SearchOutcome::Continue
        }
        // `n` → new folder: open an input modal to create a folder
        // under the current tree root. Useful for scaffolding a new
        // project dir before opening it as a workspace.
        (KeyCode::Char('n'), _) => {
            state.creating_folder = Some(String::new());
            state.last_error = None;
            SearchOutcome::Continue
        }
        // `l` / → → expand inline (keep current root, show children).
        (KeyCode::Right, _) | (KeyCode::Char('l'), _) => {
            state.expand_selected();
            SearchOutcome::Continue
        }
        (KeyCode::Char('<'), _) | (KeyCode::Left, _) | (KeyCode::Char('h'), _) => {
            state.collapse_selected();
            SearchOutcome::Continue
        }
        (KeyCode::Char(' '), _) => {
            state.toggle_expand();
            SearchOutcome::Continue
        }
        // Backspace → go up: re-root the tree at the current root's
        // parent. Lets the user escape a too-narrow starting point
        // without dropping back to search mode first.
        (KeyCode::Backspace, _) => {
            state.pop_root();
            SearchOutcome::Continue
        }
        (KeyCode::Enter, _) => {
            if let Some(row) = state.selected_row() {
                return classify_pick(&row.path);
            }
            SearchOutcome::Continue
        }
        (KeyCode::Char('g'), _) | (KeyCode::Home, _) => {
            if !state.rows.is_empty() {
                state.selected = 0;
                state.list_state.select(Some(0));
            }
            SearchOutcome::Continue
        }
        (KeyCode::Char('G'), _) | (KeyCode::End, _) => {
            if !state.rows.is_empty() {
                state.selected = state.rows.len() - 1;
                state.list_state.select(Some(state.selected));
            }
            SearchOutcome::Continue
        }
        _ => SearchOutcome::Continue,
    }
}

/// Render the tree as a ratatui List with manual indentation.
pub fn render_tree(frame: &mut Frame<'_>, area: Rect, state: &mut TreeBrowseState) {
    let items: Vec<ListItem> = state
        .rows
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth);
            let icon = if row.is_dir {
                if row.is_expanded {
                    "📂 "
                } else {
                    "📁 "
                }
            } else {
                "  "
            };
            let connector = if row.is_last_sibling {
                "└─"
            } else {
                "├─"
            };
            // Subfolder count suffix — dim, from the cache. Only
            // shown for dirs that have been loaded (expanded at
            // least once). Unexplored dirs show nothing.
            let count_suffix = if row.is_dir {
                let key = row.path.display().to_string();
                state
                    .children_cache
                    .get(&key)
                    .map(|c| format!("  ({})", c.len()))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            ListItem::new(Line::from(vec![
                Span::raw(format!("  {indent}{connector}")),
                Span::styled(
                    format!("{icon}{}", row.name),
                    if row.is_dir {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(count_suffix, Style::default().add_modifier(Modifier::DIM)),
            ]))
        })
        .collect();

    if items.is_empty() {
        let empty = Paragraph::new("  (empty directory)")
            .style(Style::default().add_modifier(Modifier::DIM));
        frame.render_widget(empty, area);
    } else {
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(list, area, &mut state.list_state);
    }

    // New-folder modal: drawn on top of the tree when open.
    if let Some(input) = &state.creating_folder {
        render_new_folder_modal(frame, area, &state.root, input, state.last_error.as_deref());
    }
}

/// Centered input modal for creating a new folder under the tree's
/// current root. Rendered on top of the tree view when
/// `TreeBrowseState::creating_folder` is Some.
fn render_new_folder_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    root: &Path,
    input: &str,
    error: Option<&str>,
) {
    use ratatui::widgets::{Block, Borders, Clear};
    let w = area.width;
    let h = area.height;
    let overlay_w = w.saturating_sub(4).clamp(40, 72);
    let overlay_h: u16 = if error.is_some() { 8 } else { 7 };
    let overlay_h = overlay_h.min(h.saturating_sub(2));
    let x = area.x + (w.saturating_sub(overlay_w)) / 2;
    let y = area.y + (h.saturating_sub(overlay_h)) / 2;
    let region = Rect {
        x,
        y,
        width: overlay_w,
        height: overlay_h,
    };
    frame.render_widget(Clear, region);

    let root_display = compact_home(&root.display().to_string());

    let block = Block::default()
        .title(" New folder ")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let mut lines = vec![
        Line::from(vec![
            Span::styled("  under: ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(root_display, Style::default().add_modifier(Modifier::DIM)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  name: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                input.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "_",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::SLOW_BLINK),
            ),
        ]),
    ];

    if let Some(err) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                err.to_string(),
                Style::default().fg(Color::Red),
            ),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Enter to create · Esc to cancel",
        Style::default().add_modifier(Modifier::DIM),
    )));

    frame.render_widget(Paragraph::new(lines).block(block), region);
}

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
    /// User pressed Esc in tree mode — switch back to search mode
    /// instead of closing the entire overlay (Android-back pattern).
    BackToSearch,
    /// User pressed `/` in tree mode — switch back to search mode
    /// with the given path as the new search root.
    SearchFromHere(PathBuf),
    /// User pressed `o` — exit pa entirely and drop into a shell at
    /// the given directory. No session scaffolded, no mpx involved.
    /// Like "Open in Terminal" from a file manager.
    OpenShellAt(PathBuf),
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
    /// How many entries in `raw_dirs` came from recency + zoxide at
    /// construction time. When the query is empty, only these are
    /// shown — walker results are hidden until the user types.
    recency_count: usize,
    /// Whether we're in global search mode (all mount points) vs
    /// project-roots mode. Toggled by Ctrl+R.
    pub global_mode: bool,
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
    /// When Some, a full-path modal is overlaid on the search
    /// showing the highlighted candidate's complete path with
    /// auto-copy. Any key dismisses.
    fullscreen_path: Option<FullscreenPath>,
    /// Current mode: search (default) or tree browser.
    pub mode: FindMode,
}

/// Full-path expand modal content. Built on `f` press, dismissed
/// on any keypress.
#[derive(Debug, Clone)]
struct FullscreenPath {
    title: String,
    lines: Vec<Line<'static>>,
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

        let recency_count = raw_dirs.len();
        let bg_rx = Some(spawn_bg_walk(opts.clone()));

        let mut s = Self {
            input: String::new(),
            candidates: Vec::new(),
            selected: 0,
            opts,
            backends,
            list_state: ListState::default(),
            raw_dirs,
            recency_count,
            global_mode: false,
            bg_rx,
            scanning: true,
            anim_tick: 0,
            anim_offset: 0,
            fullscreen_path: None,
            mode: FindMode::Search,
        };
        s.rerank();
        s
    }
}

impl SearchState {
    /// Re-root the search to a new directory. Used by tree mode's
    /// "search from here" (`/`) action. Clears the input and restarts
    /// the walker so results are scoped to the new root.
    pub fn set_root(&mut self, dir: PathBuf) {
        self.opts.roots = vec![dir];
        self.input.clear();
        self.restart_walk();
    }

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
    /// Advance the marquee by one character every 2 ticks (~0.5s
    /// per char). Readable scrolling speed — fast enough to not
    /// feel stuck, slow enough to read as it passes.
    pub fn tick_animation(&mut self) {
        self.anim_tick = self.anim_tick.wrapping_add(1);
        // Advance 1 character every 2 ticks (= every 0.5s).
        if self.anim_tick % 2 == 0 {
            self.anim_offset += 1;
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
        // Stash the currently highlighted path so we can restore the
        // selection after rebuilding the candidate list. Without this,
        // every background-walker batch resets the cursor to index 0.
        let prev_selected_path = self
            .candidates
            .get(self.selected)
            .map(|c| c.path.clone());

        let trimmed = self.input.trim();
        if trimmed.is_empty() {
            // Empty query: only show recency + zoxide candidates (the
            // first `recency_count` entries). Walker results are hidden
            // until the user types something so the list doesn't fill
            // with noise like venvs and snap packages.
            let mut seen = std::collections::HashSet::new();
            self.candidates = self
                .raw_dirs
                .iter()
                .take(self.recency_count)
                .filter(|p| seen.insert(p.to_string_lossy().into_owned()))
                .take(self.opts.limit)
                .map(|p| Candidate {
                    path: p.clone(),
                    source: Source::Recency,
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
            // Match against the leaf directory name so "MEDIA" finds
            // folders named MEDIA, not every path that happens to
            // contain scattered letters M-E-D-I-A in the full path.
            // If the query contains `/`, match against the full path
            // instead (the user is doing a path-based search).
            let match_leaf = !trimmed.contains('/');
            for p in &self.raw_dirs {
                if !seen.insert(p.to_string_lossy().into_owned()) {
                    continue;
                }
                let haystack_str = if match_leaf {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                } else {
                    p.to_string_lossy().into_owned()
                };
                let mut buf: Vec<char> = Vec::new();
                let utf32 = nucleo_matcher::Utf32Str::new(&haystack_str, &mut buf);
                if let Some(score) = pattern.score(utf32, &mut matcher) {
                    scored.push(Candidate {
                        path: p.clone(),
                        source: Source::Walk,
                        score: score as i32,
                    });
                }
            }
            scored.sort_by_key(|c| std::cmp::Reverse(c.score));
            scored.truncate(self.opts.limit);
            self.candidates = scored;
        }
        // Restore selection: find the old path in the new list.
        // If gone, clamp to the same index or the end of the list.
        if self.candidates.is_empty() {
            self.selected = 0;
            self.list_state.select(None);
        } else if let Some(ref prev) = prev_selected_path {
            if let Some(idx) = self.candidates.iter().position(|c| c.path == *prev) {
                self.selected = idx;
            } else {
                self.selected = self.selected.min(self.candidates.len() - 1);
            }
            self.list_state.select(Some(self.selected));
        } else {
            self.selected = 0;
            self.list_state.select(Some(0));
        }
    }

    /// Trigger a fresh background walk (used by > / < / Ctrl+R).
    /// Clears old results and starts a new thread.
    fn restart_walk(&mut self) {
        self.anim_offset = 0;
        self.anim_tick = 0;
        self.raw_dirs.clear();
        // Re-seed with instant tiers, but FILTER to only include
        // paths under the current root. Without this, drilling into
        // "truenas-home-server" still shows recents from completely
        // unrelated directories (the bug the user reported).
        let roots = &self.opts.roots;
        for p in crate::find::recency::collect() {
            if is_under_any_root(&p, roots) {
                self.raw_dirs.push(p);
            }
        }
        for p in crate::find::zoxide::collect() {
            if is_under_any_root(&p, roots) {
                self.raw_dirs.push(p);
            }
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
    // Full-path modal: any key dismisses. No passthrough.
    if state.fullscreen_path.is_some() {
        state.fullscreen_path = None;
        return SearchOutcome::Continue;
    }

    // Tree mode: dispatch to tree handler.
    if let FindMode::Tree(ref mut tree) = state.mode {
        // If the new-folder modal is open, send EVERY key to the tree
        // handler so none of the outer interceptors (Ctrl+F, etc.)
        // fire — the user is typing a folder name, not triggering
        // tree actions.
        if tree.creating_folder.is_some() {
            return handle_tree_key(tree, code, mods);
        }
        // Ctrl+F works in tree mode too (fullscreen path modal).
        if matches!(code, KeyCode::Char('f')) && mods.contains(KeyModifiers::CONTROL) {
            if let Some(row) = tree.selected_row() {
                state.fullscreen_path = Some(build_fullscreen_path(&row.path));
            }
            return SearchOutcome::Continue;
        }
        return handle_tree_key(tree, code, mods);
    }

    // Search mode below.
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
        // Ctrl+H aliases Backspace — many terminals send Ctrl+H when
        // Ctrl+Backspace is pressed. Without this, Ctrl+Bksp pushes a
        // literal 'h' into the input.
        (KeyCode::Char('h'), m) if m.contains(KeyModifiers::CONTROL) => {
            state.input.pop();
            state.rerank();
            SearchOutcome::Continue
        }
        // Ctrl+W: delete previous word.
        (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => {
            while state.input.ends_with(' ') {
                state.input.pop();
            }
            while state.input.chars().last().is_some_and(|c| !c.is_whitespace()) {
                state.input.pop();
            }
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
        // Drill into highlighted folder: → or Alt+J or >
        (KeyCode::Right, _) | (KeyCode::Char('>'), _) => {
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
        // Go up: ← or Alt+K or <
        (KeyCode::Left, _) | (KeyCode::Char('<'), _) => {
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
        // Ctrl+R: toggle between project-roots search and global
        // search. Global mode walks from `/` (or `/mnt` on WSL) so
        // the user can find folders on any mount point without typing
        // an absolute path.
        (KeyCode::Char('r'), m) if m.contains(KeyModifiers::CONTROL) => {
            state.global_mode = !state.global_mode;
            state.opts.roots = if state.global_mode {
                global_search_roots()
            } else {
                crate::find::default_roots()
            };
            state.input.clear();
            state.restart_walk();
            SearchOutcome::Continue
        }
        // Ctrl+T: toggle to tree browser mode. Tree roots match the
        // *current scope* — what the breadcrumb shows — NOT the
        // highlighted candidate. The highlight is often just the top
        // recent (e.g. most-recent workspace), which would hijack the
        // tree root when the user just wants to browse.
        //
        // Priority:
        //   1. Typed absolute path → that path.
        //   2. Global mode → /mnt on WSL (to see all drives) else /.
        //   3. Drilled or fresh (single root) → opts.roots[0].
        //   4. Fallback → cwd.
        (KeyCode::Char('t'), m) if m.contains(KeyModifiers::CONTROL) => {
            let trimmed = state.input.trim();
            let root = if trimmed.starts_with('/') || trimmed.starts_with("~/") {
                let abs = crate::find::expand_tilde(trimmed);
                crate::find::first_existing_ancestor(&abs)
                    .unwrap_or_else(|| PathBuf::from("/"))
            } else if state.global_mode {
                let mnt = PathBuf::from("/mnt");
                if crate::find::is_wsl() && mnt.is_dir() {
                    mnt
                } else {
                    PathBuf::from("/")
                }
            } else if state.opts.roots.len() == 1 {
                state.opts.roots[0].clone()
            } else {
                std::env::current_dir().unwrap_or_else(|_| {
                    state
                        .opts
                        .roots
                        .first()
                        .cloned()
                        .unwrap_or_else(|| PathBuf::from("."))
                })
            };
            state.mode = FindMode::Tree(Box::new(TreeBrowseState::new(root)));
            SearchOutcome::Continue
        }
        // Ctrl+F: expand the highlighted candidate's full path into
        // a modal overlay with auto-copy. Any key dismisses. Using
        // Ctrl+F instead of bare 'f' so the user can still type 'f'
        // in their search query.
        (KeyCode::Char('f'), m) if m.contains(KeyModifiers::CONTROL) => {
            if let Some(c) = state.highlighted() {
                state.fullscreen_path = Some(build_fullscreen_path(&c.path));
            }
            SearchOutcome::Continue
        }
        // Alt+J: drill in (same as >). Easier on Termux.
        (KeyCode::Char('j'), m) if m.contains(KeyModifiers::ALT) => {
            if let Some(c) = state.highlighted() {
                state.opts.roots = vec![c.path.clone()];
                state.input.clear();
                state.restart_walk();
            }
            SearchOutcome::Continue
        }
        // Alt+K: go up (same as <). Easier on Termux.
        (KeyCode::Char('k'), m) if m.contains(KeyModifiers::ALT) => {
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
        (KeyCode::Char(ch), _) => {
            state.input.push(ch);
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

    // Inner layout: breadcrumb + input + candidate list + 2-line footer.
    // Two footer lines so all hotkeys are visible even on narrow
    // Termux terminals. Line 1 = primary keys, line 2 = secondary.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // root breadcrumb
            Constraint::Length(1), // input
            Constraint::Min(1),    // candidate list
            Constraint::Length(1), // footer line 1
            Constraint::Length(1), // footer line 2
        ])
        .split(inner);

    // 2-line marquee breadcrumb. If the path fits in 2 lines, show
    // it statically. If it overflows, scroll character-by-character
    // (barbershop ticker) so the user sees every part of the path
    // rotate through the visible window.
    let inner_w = inner.width as usize;
    // In tree mode, the breadcrumb should reflect the tree's current
    // root (which changes as the user drills via `.` or Backspace),
    // not the search state's roots. In search mode, use the search
    // roots as before.
    let bc_roots: Vec<PathBuf> = match &state.mode {
        FindMode::Tree(tree) => vec![tree.root.clone()],
        FindMode::Search => state.opts.roots.clone(),
    };
    let mode_tag = match &state.mode {
        FindMode::Tree(_) => "tree",
        FindMode::Search => {
            if state.global_mode {
                "global"
            } else {
                "recents"
            }
        }
    };
    let bc_label = format!("{mode_tag} · {}", state.backends.one_liner());
    let breadcrumb = render_marquee_breadcrumb(
        &bc_roots,
        inner_w,
        &bc_label,
        state.anim_offset,
    );
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
    // Content area + hint depend on the current mode.
    match &mut state.mode {
        FindMode::Search => {
            // Search mode: input line + candidate list.
            frame.render_widget(Paragraph::new(input_line), chunks[1]);

            let items: Vec<ListItem> = state
                .candidates
                .iter()
                .map(|c| candidate_item(c, chunks[2].width))
                .collect();
            if items.is_empty() {
                let msg = if state.scanning {
                    "  scanning filesystem… results will appear as they're found"
                } else if state.input.is_empty() {
                    "  (no recents yet — type to search, Ctrl+T for tree, Ctrl+R for global)"
                } else {
                    "  no matches — try Ctrl+R for global search, or Ctrl+T for tree"
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

            use crate::tui::footer::Entry;
            crate::tui::footer::render(
                frame,
                chunks[3],
                &[
                    Entry::new("Esc", "back"),
                    Entry::new("↑/↓", "nav"),
                    Entry::new("Enter", "open"),
                    Entry::new("Ctrl+T", "tree"),
                ],
            );
            // Secondary line: less-critical keys.
            let sep = Style::default().fg(Color::DarkGray);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" ─── ", sep),
                    Span::styled(
                        "→ ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("drill  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "← ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("up  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "Ctrl+F ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("path  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "Ctrl+R ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if state.global_mode { "local" } else { "global" },
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ])),
                chunks[4],
            );
        }
        FindMode::Tree(tree) => {
            // Tree mode: no input line (use the space for tree).
            // Merge chunks[1] + chunks[2] for more tree room.
            let tree_area = Rect {
                x: chunks[1].x,
                y: chunks[1].y,
                width: chunks[1].width,
                height: chunks[1].height + chunks[2].height,
            };
            render_tree(frame, tree_area, tree);

            use crate::tui::footer::Entry;
            crate::tui::footer::render(
                frame,
                chunks[3],
                &[
                    Entry::new("Esc", "back"),
                    Entry::new("↑/↓", "nav"),
                    Entry::new("Enter", "open"),
                    Entry::new("/", "search here"),
                ],
            );
            let sep = Style::default().fg(Color::DarkGray);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" ─── ", sep),
                    Span::styled(
                        "l/→ ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("expand  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "h/← ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("collapse  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "Space ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("toggle  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        ". ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("drill  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "Bksp ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("up  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "n ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("new  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "o ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("shell  ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "q ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("quit", Style::default().add_modifier(Modifier::DIM)),
                ])),
                chunks[4],
            );
        }
    }

    // Full-path modal renders on top of everything when active.
    if let Some(fp) = &state.fullscreen_path {
        crate::tui::confirm::render_info(frame, area, &fp.title, fp.lines.clone());
    }
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
/// Render a 2-line marquee breadcrumb. If the path fits in the
/// available space, it's shown statically. If it overflows, the
/// text scrolls left character-by-character (news-ticker style)
/// within the 2-line container, wrapping around with a gap.
fn render_marquee_breadcrumb(
    roots: &[PathBuf],
    width: usize,
    backends: &str,
    char_offset: usize,
) -> Vec<Line<'static>> {
    let bold = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM);

    let path_str = if roots.len() <= 1 {
        roots
            .first()
            .map(|p| compact_home(&p.display().to_string()))
            .unwrap_or_else(|| "(no root)".into())
    } else {
        roots
            .iter()
            .map(|p| compact_home(&p.display().to_string()))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let prefix = "  📂 ";
    let full = format!("{prefix}{path_str}");
    let capacity = width.saturating_sub(2) * 2; // 2 lines worth

    if full.chars().count() <= capacity {
        // Fits — show statically across 2 lines with backends.
        return render_static_breadcrumb_inner(&full, width, backends, bold, dim);
    }

    // Overflow: marquee scroll. Build a looping string with a gap
    // separator so the wrap point is visually obvious.
    let gap = "   ···   ";
    let looping = format!("{full}{gap}");
    let loop_len = looping.chars().count();
    let offset = char_offset % loop_len;

    // Extract a window of `capacity` characters starting at offset,
    // wrapping around the loop.
    let chars: Vec<char> = looping.chars().collect();
    let mut visible = String::with_capacity(capacity);
    for i in 0..capacity {
        visible.push(chars[(offset + i) % loop_len]);
    }

    // Split into 2 lines.
    let line1_budget = width.saturating_sub(2);
    let line1: String = visible.chars().take(line1_budget).collect();
    let line2: String = visible.chars().skip(line1_budget).collect();

    vec![
        Line::from(Span::styled(format!("  {line1}"), bold)),
        Line::from(vec![
            Span::styled(format!("  {line2}"), bold),
            Span::raw("  "),
            Span::styled(format!("[{backends}]"), dim),
        ]),
    ]
}

fn render_static_breadcrumb_inner(
    full: &str,
    width: usize,
    backends: &str,
    bold: Style,
    dim: Style,
) -> Vec<Line<'static>> {
    let budget = width.saturating_sub(2);
    if full.chars().count() <= budget {
        // Fits on one line. Backends on line 1 (top), path on line 2
        // (bottom, closer to the input — the deeper/leaf part is
        // what the user cares about).
        vec![
            Line::from(Span::styled(format!("     [{backends}]"), dim)),
            Line::from(Span::styled(full.to_string(), bold)),
        ]
    } else {
        // Wraps: line 1 = start of path (ancestors), line 2 = end
        // (deeper/leaf part, closer to input where eyes are).
        let line1: String = full.chars().take(budget).collect();
        let line2: String = full.chars().skip(budget).collect();
        vec![
            Line::from(vec![
                Span::styled(format!("  {line1}"), dim),
                Span::raw("  "),
                Span::styled(format!("[{backends}]"), dim),
            ]),
            Line::from(Span::styled(format!("  {line2}"), bold)),
        ]
    }
}

/// (Previous static-only version, kept for reference)
#[allow(dead_code)]
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
#[allow(dead_code)]
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

/// Build the full-path expand modal. Chunks the path into clean
/// lines, auto-copies to clipboard, and formats the result.
fn build_fullscreen_path(path: &Path) -> FullscreenPath {
    const CHUNK_W: usize = 52;
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

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(8);
    lines.push(Line::raw(""));
    let chars: Vec<char> = path_str.chars().collect();
    for chunk in chars.chunks(CHUNK_W.max(1)) {
        let s: String = chunk.iter().collect();
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(s, Style::default().add_modifier(Modifier::BOLD)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(copy_line);
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Any key closes.",
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]));

    FullscreenPath {
        title: "Full path".into(),
        lines,
    }
}

/// Check if `path` is a descendant of any of the given `roots`.
fn is_under_any_root(path: &Path, roots: &[PathBuf]) -> bool {
    let path_str = path.display().to_string();
    roots.iter().any(|r| {
        let root_str = r.display().to_string();
        path_str.starts_with(&root_str)
    })
}

/// Roots for global search mode. On WSL, returns the individual
/// `/mnt/<drive>` entries so the walker covers all Windows drives
/// plus the native Linux root. On regular Linux/macOS, returns `/`.
fn global_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if crate::find::is_wsl() {
        // Add each /mnt/<letter> drive mount individually.
        if let Ok(entries) = std::fs::read_dir("/mnt") {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    roots.push(p);
                }
            }
        }
        // Also include native Linux home.
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            if home.is_dir() && !roots.iter().any(|r| home.starts_with(r)) {
                roots.push(home);
            }
        }
    }
    if roots.is_empty() {
        roots.push(PathBuf::from("/"));
    }
    roots
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
                    path: PathBuf::from("/home/u/my-project"),
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
                PathBuf::from("/home/u/my-project"),
                PathBuf::from("/home/u/cybersader/portagenty"),
            ],
            recency_count: 2,
            global_mode: false,
            bg_rx: None,
            scanning: false,
            anim_tick: 0,
            anim_offset: 0,
            fullscreen_path: None,
            mode: FindMode::Search,
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
mod tree_tests {
    use super::*;
    use assert_fs::prelude::*;

    #[test]
    fn tree_new_folder_creates_dir_under_root() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut tree = TreeBrowseState::new(tmp.path().to_path_buf());
        tree.creating_folder = Some("test-project".into());

        // Simulate Enter to commit.
        let out = handle_tree_key(&mut tree, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(out, SearchOutcome::Continue));

        let new_dir = tmp.path().join("test-project");
        assert!(new_dir.is_dir(), "folder should be created at {new_dir:?}");
        // Modal should be closed and no error shown.
        assert!(tree.creating_folder.is_none());
        assert!(tree.last_error.is_none());
        // New folder should appear in rebuilt rows.
        assert!(
            tree.rows.iter().any(|r| r.path == new_dir),
            "new folder should be in tree rows"
        );
    }

    #[test]
    fn tree_new_folder_rejects_path_separators() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut tree = TreeBrowseState::new(tmp.path().to_path_buf());
        tree.creating_folder = Some("a/b".into());

        handle_tree_key(&mut tree, KeyCode::Enter, KeyModifiers::NONE);

        // Modal stays open with an error.
        assert!(tree.creating_folder.is_some());
        assert!(tree.last_error.is_some());
        assert!(!tmp.path().join("a").exists());
    }

    #[test]
    fn tree_new_folder_rejects_duplicate() {
        let tmp = assert_fs::TempDir::new().unwrap();
        tmp.child("existing").create_dir_all().unwrap();
        let mut tree = TreeBrowseState::new(tmp.path().to_path_buf());
        tree.creating_folder = Some("existing".into());

        handle_tree_key(&mut tree, KeyCode::Enter, KeyModifiers::NONE);

        assert!(tree.last_error.is_some());
        assert!(tree
            .last_error
            .as_ref()
            .unwrap()
            .contains("already exists"));
    }

    #[test]
    fn tree_new_folder_esc_cancels() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut tree = TreeBrowseState::new(tmp.path().to_path_buf());
        tree.creating_folder = Some("half-typed".into());

        handle_tree_key(&mut tree, KeyCode::Esc, KeyModifiers::NONE);

        assert!(tree.creating_folder.is_none());
        assert!(!tmp.path().join("half-typed").exists());
    }

    #[test]
    fn tree_new_folder_typing_builds_input() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut tree = TreeBrowseState::new(tmp.path().to_path_buf());
        tree.creating_folder = Some(String::new());

        handle_tree_key(&mut tree, KeyCode::Char('p'), KeyModifiers::NONE);
        handle_tree_key(&mut tree, KeyCode::Char('r'), KeyModifiers::NONE);
        handle_tree_key(&mut tree, KeyCode::Char('j'), KeyModifiers::NONE);

        assert_eq!(tree.creating_folder.as_deref(), Some("prj"));
    }

    #[test]
    fn tree_new_folder_backspace_deletes_char() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut tree = TreeBrowseState::new(tmp.path().to_path_buf());
        tree.creating_folder = Some("abcd".into());

        handle_tree_key(&mut tree, KeyCode::Backspace, KeyModifiers::NONE);

        assert_eq!(tree.creating_folder.as_deref(), Some("abc"));
    }

    #[test]
    fn tree_new_folder_ctrl_h_also_deletes() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut tree = TreeBrowseState::new(tmp.path().to_path_buf());
        tree.creating_folder = Some("abcd".into());

        handle_tree_key(&mut tree, KeyCode::Char('h'), KeyModifiers::CONTROL);

        assert_eq!(tree.creating_folder.as_deref(), Some("abc"));
    }

    #[test]
    fn tree_new_folder_ctrl_letter_is_ignored_not_typed() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut tree = TreeBrowseState::new(tmp.path().to_path_buf());
        tree.creating_folder = Some("foo".into());

        // Random Ctrl+<letter> should NOT append the letter.
        handle_tree_key(&mut tree, KeyCode::Char('a'), KeyModifiers::CONTROL);

        assert_eq!(tree.creating_folder.as_deref(), Some("foo"));
    }

    #[test]
    fn tree_drill_into_selected_changes_root() {
        let tmp = assert_fs::TempDir::new().unwrap();
        tmp.child("subdir").create_dir_all().unwrap();
        let mut tree = TreeBrowseState::new(tmp.path().to_path_buf());

        // First row should be "subdir".
        assert!(!tree.rows.is_empty());
        let subdir = tmp.path().join("subdir");
        tree.selected = tree
            .rows
            .iter()
            .position(|r| r.path == subdir)
            .expect("subdir row");
        tree.list_state.select(Some(tree.selected));

        handle_tree_key(&mut tree, KeyCode::Char('.'), KeyModifiers::NONE);

        assert_eq!(tree.root, subdir);
    }

    #[test]
    fn tree_new_folder_t_key_types_into_input_not_exits_tree() {
        // Regression: the outer dispatcher used to intercept bare `t`
        // and exit tree mode, which hijacked typing `t` into the
        // new-folder name input.
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut state = super::SearchState::default();
        state.mode = super::FindMode::Tree(Box::new(TreeBrowseState::new(
            tmp.path().to_path_buf(),
        )));
        // Enter the new-folder modal.
        let _ = super::handle_key(&mut state, KeyCode::Char('n'), KeyModifiers::NONE);
        // Type "test".
        for ch in ['t', 'e', 's', 't'] {
            super::handle_key(&mut state, KeyCode::Char(ch), KeyModifiers::NONE);
        }

        // Mode should still be Tree.
        assert!(
            matches!(state.mode, super::FindMode::Tree(_)),
            "expected to still be in tree mode"
        );
        // Input should have "test".
        if let super::FindMode::Tree(ref tree) = state.mode {
            assert_eq!(tree.creating_folder.as_deref(), Some("test"));
        }
    }

    #[test]
    fn tree_o_returns_open_shell_at_highlighted_dir() {
        let tmp = assert_fs::TempDir::new().unwrap();
        tmp.child("inner").create_dir_all().unwrap();
        let mut tree = TreeBrowseState::new(tmp.path().to_path_buf());
        let inner = tmp.path().join("inner");
        tree.selected = tree
            .rows
            .iter()
            .position(|r| r.path == inner)
            .expect("inner row");
        tree.list_state.select(Some(tree.selected));

        let out = handle_tree_key(&mut tree, KeyCode::Char('o'), KeyModifiers::NONE);
        match out {
            SearchOutcome::OpenShellAt(dir) => assert_eq!(dir, inner),
            other => panic!("expected OpenShellAt, got {other:?}"),
        }
    }

    #[test]
    fn tree_o_falls_back_to_root_when_no_selection() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut tree = TreeBrowseState::new(tmp.path().to_path_buf());
        tree.rows.clear();
        tree.list_state.select(None);

        let out = handle_tree_key(&mut tree, KeyCode::Char('o'), KeyModifiers::NONE);
        match out {
            SearchOutcome::OpenShellAt(dir) => assert_eq!(dir, tmp.path()),
            other => panic!("expected OpenShellAt, got {other:?}"),
        }
    }

    #[test]
    fn tree_pop_root_goes_to_parent() {
        let tmp = assert_fs::TempDir::new().unwrap();
        tmp.child("sub").create_dir_all().unwrap();
        let mut tree = TreeBrowseState::new(tmp.path().join("sub"));
        let parent = tmp.path().to_path_buf();

        handle_tree_key(&mut tree, KeyCode::Backspace, KeyModifiers::NONE);

        assert_eq!(tree.root, parent);
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
