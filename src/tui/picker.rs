//! Workspace picker TUI. Runs before the session-list TUI when the
//! user invokes `pa` from a directory with no walkable workspace but
//! has registered workspaces globally. Keeps the UI consistent —
//! everything is rendered via ratatui, no stdin text prompts.
//!
//! Intentionally tiny: own event loop, own render, no sharing with
//! `app::App`. The two screens have different data shapes (workspaces
//! vs sessions) so folding them into one widget would mean more
//! conditionals than code. Keeping them separate is easier to read.

use std::path::{Path, PathBuf};

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    DefaultTerminal,
};

/// RAII: disables terminal mouse capture on drop, so every exit path
/// out of the picker (including a `?` early-return or an unwinding
/// panic) leaves the terminal clean — no stray scroll escape codes in
/// the user's shell afterwards. Harmless if capture was never on.
struct MouseCaptureGuard;

impl Drop for MouseCaptureGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
}

/// Enable or disable terminal mouse capture. Best-effort — a failure
/// just means the terminal doesn't support it and keyboard nav (the
/// guaranteed path) is unaffected.
fn set_mouse_capture(on: bool) {
    let _ = if on {
        crossterm::execute!(std::io::stdout(), EnableMouseCapture)
    } else {
        crossterm::execute!(std::io::stdout(), DisableMouseCapture)
    };
}

/// What the picker returned.
#[derive(Debug, Clone)]
pub enum PickerOutcome {
    /// User picked a workspace file. Caller should load it.
    Workspace(PathBuf),
    /// User picked "browse live sessions on this machine".
    LiveBrowse,
    /// User bailed (q / Esc). Caller should exit cleanly.
    Quit,
    /// User pressed `o` in the find overlay — exit pa and spawn a
    /// plain shell at the given directory. No mpx, no session.
    OpenShellAt(PathBuf),
}

/// How long status messages stick around before auto-clearing.
const STATUS_TTL: std::time::Duration = std::time::Duration::from_millis(2500);

/// Footer status line with auto-clear support.
#[derive(Debug, Default)]
struct StatusLine {
    text: Option<String>,
    set_at: Option<std::time::Instant>,
}

impl StatusLine {
    fn set(&mut self, msg: String) {
        self.text = Some(msg);
        self.set_at = Some(std::time::Instant::now());
    }
    fn clear(&mut self) {
        self.text = None;
        self.set_at = None;
    }
    fn age_out(&mut self) {
        if let Some(set_at) = self.set_at {
            if set_at.elapsed() >= STATUS_TTL {
                self.clear();
            }
        }
    }
}

/// One target of a mass-kill operation: the mpx session name to
/// kill, plus whether it was declared in the workspace TOML
/// (`tracked`) or only discovered live in the multiplexer
/// (`untracked` — leaked from outside `pa` but still under the
/// workspace's prefix). Surfaced in the confirm prompt so the user
/// sees exactly what dies.
#[derive(Debug, Clone)]
struct KillTarget {
    /// Multiplexer session name (post-`workspace_session_name`).
    mpx_name: String,
    /// Display label — the declared TOML session name for tracked
    /// entries, or the bare mpx name (minus workspace prefix when
    /// known) for untracked. Used only in the confirm prompt.
    display: String,
    tracked: bool,
}

/// Destructive action awaiting user confirmation in the picker.
#[derive(Debug, Clone)]
enum PickerPending {
    /// Drop the row from the global `[[workspace]]` registry. Leaves
    /// the `.portagenty.toml` file on disk untouched — only the index
    /// loses the entry.
    Unregister(PathBuf),
    /// Delete the workspace file from disk *and* drop the registry
    /// entry. Files-on-disk are gone after this.
    DeleteFile(PathBuf),
    /// Confirm scaffolding a new workspace at the given directory.
    /// Triggered from the find overlay's Enter on a no-workspace
    /// folder. On confirm: scaffold + open the new workspace.
    ScaffoldAt(PathBuf),
    /// Rename the workspace's display name. Holds the file path and
    /// the user's in-progress input. Enter commits, Esc cancels.
    Rename { path: PathBuf, input: String },
    /// Edit the workspace's tags (comma-separated). Holds the file
    /// path + in-progress input, seeded with the current tags. Enter
    /// writes the TOML `tags` array; Esc cancels.
    EditTags { path: PathBuf, input: String },
    /// Kill every live mpx session belonging to this workspace.
    /// Triggered by `X` in the picker. Holds the workspace's display
    /// name (for the prompt title), the resolved multiplexer (to
    /// dispatch kill calls), and the list of targets — pre-computed
    /// at key-press time so the confirm prompt names exactly what's
    /// about to die.
    KillAllSessions {
        ws_display_name: String,
        mpx: crate::domain::Multiplexer,
        targets: Vec<KillTarget>,
    },
}

/// Sticky info modal contents. Distinct from `PickerPending` because
/// info modals are non-destructive and dismissed without a y/N
/// classifier.
#[derive(Debug, Clone)]
struct InfoModal {
    title: String,
    /// Pre-rendered lines so callers can decide colors.
    lines: Vec<Line<'static>>,
    /// If set, the modal is tied to this workspace file. Used for
    /// in-modal actions like `o` → open shell at the workspace's dir.
    workspace_path: Option<PathBuf>,
}

/// Which list the picker is currently showing. Default is the
/// active workspaces; `A` toggles to the archived list and back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerView {
    Active,
    Archived,
}

/// Everything `render` needs, bundled to avoid a dozen positional
/// args. Borrows only; the picker owns the state.
struct RenderCtx<'a> {
    workspaces: &'a [PathBuf],
    /// Indices into `workspaces` that pass the active filters, in
    /// display order.
    visible: &'a [usize],
    meta: &'a std::collections::HashMap<PathBuf, WsMeta>,
    help_open: bool,
    pending: &'a Option<PickerPending>,
    info: &'a Option<InfoModal>,
    status: &'a Option<String>,
    live_counts: &'a std::collections::HashMap<PathBuf, usize>,
    view: PickerView,
    tag_filter: Option<&'a str>,
    text_filter: Option<&'a str>,
    has_sentinel: bool,
}

/// Run the picker inside an already-initialized ratatui terminal.
/// Terminal init + restore stay with the caller so a single
/// `ratatui::init()` handles both the picker and the session-list
/// TUI that follows — no flicker from tearing down between them.
///
/// `active` is the default list; `archived` holds workspaces the
/// user hid with `a`. `A` toggles which list is shown. The two
/// buckets swap into `workspaces` on toggle so all the existing
/// index/render logic operates on one list unchanged — only the
/// live-sessions sentinel (active view only) is view-dependent.
pub fn run(
    terminal: &mut DefaultTerminal,
    active: &[PathBuf],
    archived: &[PathBuf],
) -> Result<PickerOutcome> {
    // Picker-local mutable copies: actions like unregister / delete /
    // archive change the lists in place without re-entering the outer
    // run loop. `workspaces` is always the *currently shown* bucket;
    // `hidden` is the other one. Toggling `A` swaps them.
    let mut workspaces: Vec<PathBuf> = active.to_vec();
    let mut hidden: Vec<PathBuf> = archived.to_vec();
    let mut view = PickerView::Active;
    let mut state = ListState::default();
    state.select(Some(0));

    let mut help_open = false;
    let mut pending: Option<PickerPending> = None;
    let mut info: Option<InfoModal> = None;
    let mut search: Option<crate::tui::find::SearchState> = None;
    let mut status = StatusLine::default();
    // Per-workspace name + tags, resolved once (refreshed after a tag
    // edit) so render + filtering never re-read the TOML per frame.
    let mut meta = build_meta(&[&workspaces, &hidden]);
    // Organizing filters over the shown list. `tag_filter` is a
    // persistent single-tag view filter (cycled with `f`).
    // `text_filter` is the incremental `/` fuzzy filter: `Some(query)`
    // means filter mode is active (query may be empty), `None` means
    // not filtering. Both narrow `visible`; the sentinel hides while
    // either is active.
    let mut tag_filter: Option<String> = None;
    let mut text_filter: Option<String> = None;
    // Opt-in mouse. Capture is enabled only while the picker is shown
    // (this loop) so the session list keeps the terminal's native
    // click-drag text selection of its commands/paths. `M` toggles it
    // live + persists. The guard disables capture on every return.
    let mut mouse_on = crate::config::ui_mouse_enabled();
    if mouse_on {
        set_mouse_capture(true);
    }
    let _mouse_guard = MouseCaptureGuard;
    // (row index, instant) of the last left-click, for double-click.
    let mut last_click: Option<(usize, std::time::Instant)> = None;
    // Live-session count per workspace, used to render a "2 live"
    // badge next to each row. Probed over both buckets so counts are
    // correct in either view; refreshed on Ctrl+R for the power user.
    let mut live_counts = {
        let mut all = workspaces.clone();
        all.extend(hidden.iter().cloned());
        compute_live_counts(&all)
    };

    loop {
        // Auto-age the status line so messages don't sit forever.
        status.age_out();
        let mut visible = compute_visible(
            &workspaces,
            &meta,
            tag_filter.as_deref(),
            text_filter.as_deref(),
        );
        // Auto-drop a tag filter that no longer matches anything —
        // its sole holder was just untagged (`t`) or archived (`a`),
        // so keeping the filter would strand the user on an empty
        // "(no matches)" view. Only when a text query isn't also
        // active: typing a query that matches nothing is normal.
        if tag_filter.is_some() && text_filter.is_none() && visible.is_empty() {
            tag_filter = None;
            state.select(Some(0));
            visible = compute_visible(&workspaces, &meta, None, None);
        }
        let filtering = tag_filter.is_some() || text_filter.is_some();
        // The live-sessions sentinel only shows in the unfiltered
        // active view — filtering is about finding a workspace.
        let has_sentinel = view == PickerView::Active && !filtering;
        // `.max(1)` keeps the wrap-around nav math (`% total`,
        // `total - 1`) panic-free when the visible list is empty.
        let total = (visible.len() + usize::from(has_sentinel)).max(1);
        // Keep the selection in range as the visible set changes.
        {
            let sel = state.selected().unwrap_or(0);
            if sel >= total {
                state.select(Some(total - 1));
            }
        }
        let rctx = RenderCtx {
            workspaces: &workspaces,
            visible: &visible,
            meta: &meta,
            help_open,
            pending: &pending,
            info: &info,
            status: &status.text,
            live_counts: &live_counts,
            view,
            tag_filter: tag_filter.as_deref(),
            text_filter: text_filter.as_deref(),
            has_sentinel,
        };
        terminal.draw(|frame| render(frame, &mut state, &mut search, rctx))?;

        // Drain background walker results + advance the breadcrumb
        // animation each render tick — even without a key press.
        if let Some(s) = search.as_mut() {
            s.poll_background();
            s.tick_animation();
        }

        if !event::poll(std::time::Duration::from_millis(250))? {
            continue;
        }
        let ev = event::read()?;
        // Mouse events only arrive while capture is on. Wheel scrolls
        // the selection; left-click selects the row under the cursor;
        // a second click on the same row within 400ms opens it. Modals
        // ignore mouse (keyboard-only), so we skip when an overlay is
        // up. Hit-test assumes uniform rows per width tier (1 line at
        // ≥70 cols, else a 2-line card) below the title + hint rows.
        if let Event::Mouse(me) = ev {
            let modal_up = help_open || info.is_some() || pending.is_some() || search.is_some();
            if mouse_on && !modal_up {
                match me.kind {
                    MouseEventKind::ScrollDown => {
                        let sel = state.selected().unwrap_or(0);
                        state.select(Some((sel + 1) % total));
                    }
                    MouseEventKind::ScrollUp => {
                        let sel = state.selected().unwrap_or(0);
                        state.select(Some(if sel == 0 { total - 1 } else { sel - 1 }));
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        let w = terminal.size().map(|s| s.width).unwrap_or(80);
                        let row_h: usize = if w >= 70 { 1 } else { 2 };
                        const LIST_TOP: u16 = 2; // title + hint rows
                        if me.row >= LIST_TOP {
                            let disp = state.offset() + (me.row - LIST_TOP) as usize / row_h;
                            if disp < total {
                                let now = std::time::Instant::now();
                                let is_double = last_click.is_some_and(|(idx, t)| {
                                    idx == disp
                                        && now.duration_since(t)
                                            < std::time::Duration::from_millis(400)
                                });
                                state.select(Some(disp));
                                if is_double {
                                    last_click = None;
                                    if has_sentinel && disp == visible.len() {
                                        return Ok(PickerOutcome::LiveBrowse);
                                    }
                                    if let Some(p) = selected_workspace(
                                        &workspaces,
                                        &visible,
                                        &state,
                                        has_sentinel,
                                    ) {
                                        return Ok(PickerOutcome::Workspace(p));
                                    }
                                } else {
                                    last_click = Some((disp, now));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            continue;
        }
        let Event::Key(key) = ev else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        // Help overlay: any key closes it. No passthrough.
        if help_open {
            help_open = false;
            continue;
        }
        // Info modal: most keys dismiss. `o` (if the modal is tied
        // to a workspace path) opens a plain shell at that dir —
        // natural next step after "reveal shows me where it is."
        if let Some(m) = info.as_ref() {
            if matches!(key.code, KeyCode::Char('o')) {
                if let Some(ws_path) = m.workspace_path.as_ref() {
                    if let Some(dir) = ws_path.parent().map(|d| d.to_path_buf()) {
                        return Ok(PickerOutcome::OpenShellAt(dir));
                    }
                }
            }
            info = None;
            continue;
        }
        // Search overlay: divert keys to the find module's handler.
        if let Some(s) = search.as_mut() {
            use crate::tui::find::SearchOutcome;
            match crate::tui::find::handle_key(s, key.code, key.modifiers) {
                SearchOutcome::Continue => {}
                SearchOutcome::Cancel => {
                    search = None;
                }
                SearchOutcome::OpenHelp => {
                    help_open = true;
                }
                SearchOutcome::OpenExisting(path) => {
                    // Picking an existing-but-unregistered workspace
                    // via `n` should register it on the way out, so
                    // it shows up in the picker list next time
                    // without the user having to walk-up + re-enter.
                    // Mirrors the auto-re-register hook in
                    // `tui::run`: register, then reconcile any
                    // previous_paths owed (e.g. the same id was
                    // previously registered at a different path —
                    // i.e. a folder move + manual re-pick via find).
                    // Best-effort: a registry-write failure must not
                    // block opening the workspace.
                    let _ = crate::config::register_global_workspace(&path);
                    let _ = crate::config::reconcile_previous_paths_on_reregister(&path);
                    return Ok(PickerOutcome::Workspace(path));
                }
                SearchOutcome::ScaffoldAt(path) => {
                    search = None;
                    pending = Some(PickerPending::ScaffoldAt(path));
                }
                SearchOutcome::BackToSearch => {
                    // Tree mode Esc → switch back to search mode.
                    if let Some(s) = search.as_mut() {
                        s.mode = crate::tui::find::FindMode::Search;
                    }
                }
                SearchOutcome::SearchFromHere(dir) => {
                    // Tree mode `/` → back to search with new root.
                    if let Some(s) = search.as_mut() {
                        s.mode = crate::tui::find::FindMode::Search;
                        s.set_root(dir);
                    }
                }
                SearchOutcome::OpenShellAt(dir) => {
                    // Tree mode `o` → exit picker, caller spawns shell.
                    return Ok(PickerOutcome::OpenShellAt(dir));
                }
            }
            continue;
        }
        // Confirm modal: divert keys. ScaffoldAt is special — on
        // confirm we exit the picker with the new workspace as the
        // outcome so the outer driver opens its session TUI right
        // away (per DESIGN §12: "scaffold + open immediately").
        if let Some(action) = pending.take() {
            // Rename + EditTags have their own text-input flow — divert
            // keys into the input field instead of routing through the
            // y/N confirm. On Enter they commit (differently); on Esc
            // they cancel; all other keys edit the shared input buffer.
            match action {
                PickerPending::Rename { path, mut input } => {
                    match key.code {
                        KeyCode::Esc => status.set("rename cancelled".into()),
                        KeyCode::Enter => match crate::workspace_edit::set_name(&path, &input) {
                            Ok(_) => {
                                meta = build_meta(&[&workspaces, &hidden]);
                                status.set(format!("renamed to {input:?}"));
                            }
                            Err(e) => status.set(format!("rename failed: {e:#}")),
                        },
                        _ => {
                            edit_text_input(&mut input, key.code, key.modifiers);
                            pending = Some(PickerPending::Rename { path, input });
                        }
                    }
                    continue;
                }
                PickerPending::EditTags { path, mut input } => {
                    match key.code {
                        KeyCode::Esc => status.set("tag edit cancelled".into()),
                        KeyCode::Enter => {
                            let tags = crate::workspace_edit::parse_tags_input(&input);
                            match crate::workspace_edit::set_tags(&path, &tags) {
                                Ok(_) => {
                                    // Refresh meta so chips + filters
                                    // reflect the new tags immediately.
                                    meta = build_meta(&[&workspaces, &hidden]);
                                    status.set(if tags.is_empty() {
                                        "tags cleared".into()
                                    } else {
                                        format!("tags: {}", tags.join(", "))
                                    });
                                }
                                Err(e) => status.set(format!("tag edit failed: {e:#}")),
                            }
                        }
                        _ => {
                            edit_text_input(&mut input, key.code, key.modifiers);
                            pending = Some(PickerPending::EditTags { path, input });
                        }
                    }
                    continue;
                }
                // Confirm-style actions (Unregister / DeleteFile /
                // ScaffoldAt / KillAllSessions) route through the y/N
                // classifier.
                other => match crate::tui::confirm::classify(key.code) {
                    crate::tui::confirm::ConfirmKey::Confirm => {
                        if let PickerPending::ScaffoldAt(dir) = &other {
                            match perform_scaffold_at(dir) {
                                Ok(new_path) => {
                                    return Ok(PickerOutcome::Workspace(new_path));
                                }
                                Err(e) => {
                                    status.set(format!("scaffold failed: {e:#}"));
                                }
                            }
                        } else {
                            // Re-probe live counts after a kill so the
                            // badge column reflects reality without
                            // waiting for Ctrl+R. Other actions don't
                            // touch live mpx state, so spare them the
                            // probe cost (~100ms per distinct mpx).
                            let was_kill = matches!(other, PickerPending::KillAllSessions { .. });
                            let msg = perform_picker_action(other, &mut workspaces, &mut state);
                            status.set(msg);
                            if was_kill {
                                live_counts = compute_live_counts(&workspaces);
                            }
                        }
                    }
                    crate::tui::confirm::ConfirmKey::Cancel => {
                        status.set("cancelled".into());
                    }
                },
            }
            continue;
        }
        // Text-filter (`/`) input mode: keys type into the query and
        // narrow the list live. Only nav + open + exit keys are
        // special; everything else edits the query. Runs before the
        // main keymap so `a`/`d`/`n`/etc. become literal query chars
        // while filtering.
        if let Some(mut query) = text_filter.take() {
            match key.code {
                KeyCode::Esc => {
                    // First Esc with a query clears it (stay in filter
                    // mode); an empty-query Esc exits filter mode.
                    if query.is_empty() {
                        // text_filter stays None → exit.
                        state.select(Some(0));
                    } else {
                        query.clear();
                        text_filter = Some(query);
                        state.select(Some(0));
                    }
                }
                KeyCode::Enter | KeyCode::Right => {
                    // The sentinel is always hidden while filtering
                    // (has_sentinel is false here), so only a real
                    // workspace match can open.
                    if let Some(p) = selected_workspace(&workspaces, &visible, &state, has_sentinel)
                    {
                        return Ok(PickerOutcome::Workspace(p));
                    }
                    text_filter = Some(query); // no match under cursor
                }
                KeyCode::Up => {
                    let sel = state.selected().unwrap_or(0);
                    state.select(Some(if sel == 0 { total - 1 } else { sel - 1 }));
                    text_filter = Some(query);
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let sel = state.selected().unwrap_or(0);
                    state.select(Some(if sel == 0 { total - 1 } else { sel - 1 }));
                    text_filter = Some(query);
                }
                KeyCode::Down => {
                    let sel = state.selected().unwrap_or(0);
                    state.select(Some((sel + 1) % total));
                    text_filter = Some(query);
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let sel = state.selected().unwrap_or(0);
                    state.select(Some((sel + 1) % total));
                    text_filter = Some(query);
                }
                _ => {
                    edit_text_input(&mut query, key.code, key.modifiers);
                    text_filter = Some(query);
                    state.select(Some(0));
                }
            }
            continue;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('?'), _) => {
                help_open = true;
            }
            (KeyCode::Char('q'), _) => return Ok(PickerOutcome::Quit),
            (KeyCode::Esc, _) => {
                // Esc precedence: dismiss a status line, then clear an
                // active tag filter, then exit pa. Each stage prevents
                // an accidental Esc from quitting with state still up.
                if status.text.is_some() {
                    status.clear();
                } else if tag_filter.is_some() {
                    tag_filter = None;
                    state.select(Some(0));
                } else {
                    return Ok(PickerOutcome::Quit);
                }
            }
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                return Ok(PickerOutcome::Quit);
            }
            // Ctrl+R: re-probe mpxs and refresh the live-session
            // badges. Useful after the user expects state to have
            // changed (e.g. started/killed a session elsewhere).
            (KeyCode::Char('r'), m) if m.contains(KeyModifiers::CONTROL) => {
                live_counts = compute_live_counts(&workspaces);
                status.set("refreshed live-session counts".into());
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
            // Ctrl+D / Ctrl+U: half-page jumps (vim-style).
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                let sel = state.selected().unwrap_or(0);
                let page = (total / 2).max(1);
                state.select(Some((sel + page).min(total - 1)));
            }
            (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                let sel = state.selected().unwrap_or(0);
                let page = (total / 2).max(1);
                state.select(Some(sel.saturating_sub(page)));
            }
            // PageDown / PageUp: full-page jumps.
            (KeyCode::PageDown, _) => {
                let sel = state.selected().unwrap_or(0);
                state.select(Some((sel + 10).min(total - 1)));
            }
            (KeyCode::PageUp, _) => {
                let sel = state.selected().unwrap_or(0);
                state.select(Some(sel.saturating_sub(10)));
            }
            // `l` / Right → open highlighted workspace (vim-style
            // "move right into the thing you're looking at").
            (KeyCode::Char('l'), _) | (KeyCode::Right, _) => {
                if has_sentinel && state.selected() == Some(visible.len()) {
                    return Ok(PickerOutcome::LiveBrowse);
                }
                if let Some(p) = selected_workspace(&workspaces, &visible, &state, has_sentinel) {
                    return Ok(PickerOutcome::Workspace(p));
                }
            }
            // / → enter incremental fuzzy filter mode over the shown
            // workspaces (name + path + tags). Keeps recency order.
            (KeyCode::Char('/'), _) => {
                text_filter = Some(String::new());
                state.select(Some(0));
            }
            (KeyCode::Char('f'), m) if m.contains(KeyModifiers::CONTROL) => {
                text_filter = Some(String::new());
                state.select(Some(0));
            }
            // f → cycle the single-tag view filter: none → tag1 → … →
            // none. A no-op (with a hint) when nothing is tagged.
            (KeyCode::Char('f'), _) => {
                let tags = distinct_tags(&workspaces, &meta);
                if tags.is_empty() {
                    status.set("f: no tags yet — press t to tag the highlighted workspace".into());
                } else {
                    tag_filter = cycle_tag_filter(tag_filter.as_deref(), &tags);
                    state.select(Some(0));
                    match &tag_filter {
                        Some(t) => status.set(format!("filter: #{t}")),
                        None => status.set("filter cleared".into()),
                    }
                }
            }
            // t → edit the highlighted workspace's tags (comma-
            // separated). Writes the committable TOML `tags` array.
            (KeyCode::Char('t'), _) => {
                if let Some(path) = selected_workspace(&workspaces, &visible, &state, has_sentinel)
                {
                    let current = meta
                        .get(&path)
                        .map(|m| m.tags.join(", "))
                        .unwrap_or_default();
                    pending = Some(PickerPending::EditTags {
                        path,
                        input: current,
                    });
                } else {
                    status.set("t: live-sessions row can't be tagged".into());
                }
            }
            // a → archive the highlighted workspace (active view) or
            // unarchive it (archived view). Non-destructive + instant:
            // archiving just hides the row from the default list so
            // long registries don't bury the workspaces you actually
            // use. The file + its registration both stay put.
            (KeyCode::Char('a'), _) => {
                let Some(ws_idx) = selected_ws_index(&visible, &state, has_sentinel) else {
                    status.set("a: the live-sessions row can't be archived".into());
                    continue;
                };
                let path = workspaces[ws_idx].clone();
                let archive = view == PickerView::Active;
                match crate::config::set_workspace_archived(&path, archive) {
                    Ok(()) => {
                        // Move the row from the shown bucket to the
                        // hidden one and keep the selection in range.
                        workspaces.remove(ws_idx);
                        hidden.push(path.clone());
                        let name = read_workspace_name(&path)
                            .unwrap_or_else(|| path.display().to_string());
                        if archive {
                            status.set(format!("archived {name:?} — press A to view archived"));
                        } else {
                            status.set(format!("unarchived {name:?}"));
                            // If the archived view just emptied, pop
                            // back to the active list automatically.
                            if workspaces.is_empty() {
                                std::mem::swap(&mut workspaces, &mut hidden);
                                view = PickerView::Active;
                                state.select(Some(0));
                            }
                        }
                    }
                    Err(e) => status.set(format!("archive failed: {e:#}")),
                }
            }
            // A → toggle between the active list and the archived
            // list. Refuses to switch to an empty archived view.
            // Clears any active filter so the swapped-in view is fresh.
            (KeyCode::Char('A'), _) => match view {
                PickerView::Active => {
                    if hidden.is_empty() {
                        status.set("A: no archived workspaces yet — press a to archive one".into());
                    } else {
                        std::mem::swap(&mut workspaces, &mut hidden);
                        view = PickerView::Archived;
                        tag_filter = None;
                        text_filter = None;
                        state.select(Some(0));
                        status.set("archived view — a to unarchive, A to go back".into());
                    }
                }
                PickerView::Archived => {
                    std::mem::swap(&mut workspaces, &mut hidden);
                    view = PickerView::Active;
                    tag_filter = None;
                    text_filter = None;
                    state.select(Some(0));
                    status.clear();
                }
            },
            (KeyCode::Char('d'), _) => {
                if let Some(path) = selected_workspace(&workspaces, &visible, &state, has_sentinel)
                {
                    pending = Some(PickerPending::Unregister(path));
                } else {
                    status.set(
                        "d: nothing to unregister — live-sessions row isn't a workspace".into(),
                    );
                }
            }
            (KeyCode::Char('D'), _) => {
                if let Some(path) = selected_workspace(&workspaces, &visible, &state, has_sentinel)
                {
                    pending = Some(PickerPending::DeleteFile(path));
                } else {
                    status.set("D: nothing to delete — live-sessions row isn't a workspace".into());
                }
            }
            // X: kill every live mpx session under the highlighted
            // workspace. Capital — mirrors `D` for delete-file: both
            // destructive sweeps, both gated by an explicit y/N
            // confirm that names exactly what's about to die. Zero
            // live sessions short-circuits to a status message, no
            // confirm needed (nothing to confirm).
            (KeyCode::Char('X'), _) => {
                let Some(path) = selected_workspace(&workspaces, &visible, &state, has_sentinel)
                else {
                    status.set(
                        "X: live-sessions row has no workspace — \
                         use `x` from inside the session list for ad-hoc kills"
                            .into(),
                    );
                    continue;
                };
                match enumerate_kill_targets(&path) {
                    Ok((ws_display_name, mpx, targets)) => {
                        if targets.is_empty() {
                            status.set(format!(
                                "X: no live sessions under {ws_display_name:?} to kill"
                            ));
                        } else {
                            let _ = path; // path was only used to load the workspace above
                            pending = Some(PickerPending::KillAllSessions {
                                ws_display_name,
                                mpx,
                                targets,
                            });
                        }
                    }
                    Err(e) => status.set(format!("X: couldn't read workspace: {e:#}")),
                }
            }
            (KeyCode::Char('r'), _) => {
                if let Some(path) = selected_workspace(&workspaces, &visible, &state, has_sentinel)
                {
                    info = Some(build_reveal_modal(&path));
                } else {
                    status.set("r: live-sessions row has no file path".into());
                }
            }
            (KeyCode::Char('n'), _) => {
                // Open the in-TUI find-folder overlay. Default opts
                // search from $HOME with depth 6, recency + zoxide
                // tiers populate the initial list before any typing.
                search = Some(crate::tui::find::SearchState::default());
            }
            (KeyCode::Char('e'), _) => {
                status.set("e: in-TUI workspace editing is coming soon".into());
            }
            // M → toggle opt-in mouse (wheel-scroll + click-select +
            // double-click-open), persisted machine-locally. Default
            // off because capture disables the terminal's own
            // click-drag copy of the paths shown in rows. Picker-only:
            // the session list keeps native text selection.
            (KeyCode::Char('M'), _) => {
                mouse_on = !mouse_on;
                set_mouse_capture(mouse_on);
                let _ = crate::config::set_ui_mouse(mouse_on);
                status.set(if mouse_on {
                    "mouse: on — wheel scrolls · click selects · dbl-click opens".into()
                } else {
                    "mouse: off — native text-selection restored".into()
                });
            }
            (KeyCode::Char('R'), _) => {
                if let Some(path) = selected_workspace(&workspaces, &visible, &state, has_sentinel)
                {
                    // Seed input with current display name so the user
                    // can tweak instead of retyping from scratch.
                    let current_name = read_workspace_name(&path).unwrap_or_default();
                    pending = Some(PickerPending::Rename {
                        path,
                        input: current_name,
                    });
                } else {
                    status.set("R: live-sessions row isn't a workspace".into());
                }
            }
            (KeyCode::Enter, _) => {
                if has_sentinel && state.selected() == Some(visible.len()) {
                    return Ok(PickerOutcome::LiveBrowse);
                }
                if let Some(p) = selected_workspace(&workspaces, &visible, &state, has_sentinel) {
                    return Ok(PickerOutcome::Workspace(p));
                }
            }
            _ => {}
        }
    }
}

/// Build the "reveal path" info modal. Auto-attempts to copy the
/// path to the system clipboard, then hard-wraps the path into
/// clean lines that fit the modal width — so a mobile long-press
/// selects exactly the visible text without trailing-padding
/// artifacts from ratatui's `Wrap` widget.
///
/// The wrap width (52) matches the modal's inner width budget:
/// overlay clamps to 60 cols, minus 2 borders and 2 chars of padding
/// per side. If we ever bump the modal width, bump this constant.
fn build_reveal_modal(path: &std::path::Path) -> InfoModal {
    const PATH_WRAP: usize = 52;
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
            Span::raw(format!(" to clipboard via `{tool}` — paste anywhere.")),
        ]),
        Err(e) => Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "couldn't auto-copy:",
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
    // Each path chunk on its own line, no trailing space, no Wrap{}
    // — long-press selection on mobile picks up exactly the visible
    // characters of the chunk.
    for chunk in chunked_path(&path_str, PATH_WRAP) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(chunk, Style::default().add_modifier(Modifier::BOLD)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(copy_line);
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "o ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "open a plain shell here (exits pa).",
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Any other key closes (Esc / q / Enter).",
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]));
    InfoModal {
        title: "Workspace path".into(),
        lines,
        workspace_path: Some(path.to_path_buf()),
    }
}

/// Chop a string into max-`width`-char chunks, returning each as an
/// owned String. Chars (not bytes) so multi-byte UTF-8 is safe.
fn chunked_path(s: &str, width: usize) -> Vec<String> {
    if s.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = s.chars().collect();
    chars
        .chunks(width.max(1))
        .map(|c| c.iter().collect::<String>())
        .collect()
}

/// Apply one readline-style edit key to `input`: Backspace, Ctrl+H
/// (alias), Ctrl+W (word), Ctrl+U (clear), or a printable char.
/// Other Ctrl+<letter> combos are ignored so they don't insert
/// literal chars. Shared by the rename + tag-edit modals and the
/// `/` filter input.
fn edit_text_input(input: &mut String, code: KeyCode, mods: KeyModifiers) {
    match code {
        KeyCode::Backspace => {
            input.pop();
        }
        KeyCode::Char('h') if mods.contains(KeyModifiers::CONTROL) => {
            input.pop();
        }
        KeyCode::Char('w') if mods.contains(KeyModifiers::CONTROL) => {
            while input.ends_with(' ') {
                input.pop();
            }
            while input.chars().last().is_some_and(|c| !c.is_whitespace()) {
                input.pop();
            }
        }
        KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
            input.clear();
        }
        KeyCode::Char(_) if mods.contains(KeyModifiers::CONTROL) => {}
        KeyCode::Char(ch) => input.push(ch),
        _ => {}
    }
}

/// Map the selected display row to an index into `workspaces`, going
/// through the visible-filter map. Returns `None` for the
/// live-sessions sentinel row or an out-of-range selection.
fn selected_ws_index(visible: &[usize], state: &ListState, has_sentinel: bool) -> Option<usize> {
    let sel = state.selected()?;
    if has_sentinel && sel == visible.len() {
        return None; // sentinel row
    }
    visible.get(sel).copied()
}

/// Resolve the currently-selected row to a workspace PathBuf, or
/// `None` when the live-sessions sentinel (or no row) is selected.
/// All the row-action keys share this guard so the sentinel is
/// handled consistently, filtered or not.
fn selected_workspace(
    workspaces: &[PathBuf],
    visible: &[usize],
    state: &ListState,
    has_sentinel: bool,
) -> Option<PathBuf> {
    let idx = selected_ws_index(visible, state, has_sentinel)?;
    workspaces.get(idx).cloned()
}

/// Execute a confirmed picker action and mutate the row list + state
/// in place. Returns a human-readable status string the UI pins in
/// the footer until the next keystroke.
fn perform_picker_action(
    action: PickerPending,
    workspaces: &mut Vec<PathBuf>,
    state: &mut ListState,
) -> String {
    match action {
        PickerPending::Unregister(path) => {
            match crate::config::unregister_global_workspace(&path) {
                Ok(()) => {
                    workspaces.retain(|p| p != &path);
                    clamp_selection(workspaces, state);
                    format!("unregistered from global index: {}", path.display())
                }
                Err(e) => format!("unregister failed: {e:#}"),
            }
        }
        PickerPending::DeleteFile(path) => {
            // Delete the file first; if successful, then drop the
            // registry entry (best-effort — a stale entry is
            // auto-filtered on next picker load anyway).
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    let _ = crate::config::unregister_global_workspace(&path);
                    workspaces.retain(|p| p != &path);
                    clamp_selection(workspaces, state);
                    format!("deleted workspace file: {}", path.display())
                }
                Err(e) => format!("delete failed: {e}"),
            }
        }
        PickerPending::KillAllSessions {
            ws_display_name,
            mpx,
            targets,
        } => perform_kill_all_sessions(&ws_display_name, mpx, &targets),
        // ScaffoldAt is handled inline in the run loop because on
        // success we exit the picker entirely, returning the new
        // workspace as the outcome. Reaching this arm is a bug.
        PickerPending::ScaffoldAt(_) => "scaffold path took the wrong branch (bug)".into(),
        // Rename + EditTags have their own input-divert flow in the
        // main loop, so these arms are unreachable in practice.
        PickerPending::Rename { .. } => "rename path took the wrong branch (bug)".into(),
        PickerPending::EditTags { .. } => "tag-edit path took the wrong branch (bug)".into(),
    }
}

/// Resolve a workspace + probe its mpx for live sessions, returning
/// the kill list with tracked / untracked labels. Tracked = declared
/// in TOML AND live in mpx. Untracked = live in mpx, prefixed with
/// the workspace's sanitized name, but NOT in the declared set —
/// i.e. leaked from outside `pa` but logically under this workspace.
/// Mirrors the session-list TUI's "show both kinds" semantics so the
/// kill verb covers the same surface the eye sees.
fn enumerate_kill_targets(
    path: &Path,
) -> anyhow::Result<(String, crate::domain::Multiplexer, Vec<KillTarget>)> {
    let ws = crate::config::load(&crate::config::LoadOptions {
        workspace_path: Some(path.to_path_buf()),
        ..Default::default()
    })?;
    let ws_name = ws.name.clone();
    let mpx = ws.multiplexer;
    let declared_names: Vec<String> = ws.sessions.iter().map(|s| s.name.clone()).collect();

    let mux: Option<Box<dyn crate::mux::Multiplexer>> = match mpx {
        crate::domain::Multiplexer::Tmux => Some(Box::new(crate::mux::TmuxAdapter::new())),
        crate::domain::Multiplexer::Zellij => Some(Box::new(crate::mux::ZellijAdapter::new())),
        crate::domain::Multiplexer::Wezterm => None,
    };
    let Some(mux) = mux else {
        return Ok((ws_name, mpx, vec![]));
    };
    let live: std::collections::HashSet<String> = match mux.list_sessions() {
        Ok(rows) => rows.into_iter().map(|s| s.name).collect(),
        Err(_) => return Ok((ws_name, mpx, vec![])),
    };

    let mut targets: Vec<KillTarget> = Vec::new();
    let mut tracked_mpx: std::collections::HashSet<String> = std::collections::HashSet::new();
    for sn in &declared_names {
        let mpx_name = crate::mux::workspace_session_name(&ws_name, sn);
        if live.contains(&mpx_name) {
            targets.push(KillTarget {
                mpx_name: mpx_name.clone(),
                display: sn.clone(),
                tracked: true,
            });
            tracked_mpx.insert(mpx_name);
        }
    }
    let prefix = format!(
        "{}-",
        crate::mux::workspace_session_name(&ws_name, "").trim_end_matches('-')
    );
    for live_name in &live {
        if tracked_mpx.contains(live_name) {
            continue;
        }
        if live_name.starts_with(&prefix) {
            let display = live_name
                .strip_prefix(&prefix)
                .unwrap_or(live_name)
                .to_string();
            targets.push(KillTarget {
                mpx_name: live_name.clone(),
                display,
                tracked: false,
            });
        }
    }
    Ok((ws_name, mpx, targets))
}

/// Iterate the kill list, invoking the right mpx adapter for each.
/// Best-effort: per-target failures append to an error tally but
/// don't abort the sweep. Returns the human-readable status line.
fn perform_kill_all_sessions(
    ws_display_name: &str,
    mpx: crate::domain::Multiplexer,
    targets: &[KillTarget],
) -> String {
    let mux: Option<Box<dyn crate::mux::Multiplexer>> = match mpx {
        crate::domain::Multiplexer::Tmux => Some(Box::new(crate::mux::TmuxAdapter::new())),
        crate::domain::Multiplexer::Zellij => Some(Box::new(crate::mux::ZellijAdapter::new())),
        crate::domain::Multiplexer::Wezterm => None,
    };
    let Some(mux) = mux else {
        return format!("X: workspace {ws_display_name:?} mpx isn't supported — nothing killed");
    };
    let mut killed = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for t in targets {
        match mux.kill(&t.mpx_name) {
            Ok(()) => killed += 1,
            Err(e) => failed.push(format!("{}: {e}", t.display)),
        }
    }
    if failed.is_empty() {
        format!(
            "killed {killed} live session{} under {ws_display_name:?}",
            if killed == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "killed {killed} of {} under {ws_display_name:?}; failed: {}",
            targets.len(),
            failed.join(", ")
        )
    }
}

/// Read the `name = "..."` field from a workspace TOML. Returns
/// `None` on any failure (missing file, parse error, missing field).
fn read_workspace_name(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let doc: toml_edit::DocumentMut = raw.parse().ok()?;
    doc.get("name")?.as_str().map(|s| s.to_string())
}

/// Per-workspace picker metadata, resolved once per picker entry (and
/// after tag edits) so render + filter don't re-read the TOML on
/// every frame. `name` falls back to the filename stem.
#[derive(Debug, Clone, Default)]
struct WsMeta {
    name: String,
    tags: Vec<String>,
}

/// Read a workspace's display name + own `tags` from its TOML in one
/// pass. Missing/unparseable files fall back to the filename stem and
/// empty tags. Uses the workspace's *own* tags (not the project-tag
/// union that `merge` computes) — those are what the picker's `t`
/// editor writes and what the user manages directly.
fn read_workspace_meta(path: &std::path::Path) -> WsMeta {
    let fallback_name = || {
        path.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".portagenty"))
            .unwrap_or_else(|| path.file_name().and_then(|s| s.to_str()).unwrap_or("?"))
            .to_string()
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return WsMeta {
            name: fallback_name(),
            tags: vec![],
        };
    };
    let Ok(doc) = raw.parse::<toml_edit::DocumentMut>() else {
        return WsMeta {
            name: fallback_name(),
            tags: vec![],
        };
    };
    let name = doc
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(fallback_name);
    let tags = doc
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    WsMeta { name, tags }
}

/// Build the path → metadata map over every workspace in both the
/// shown and hidden buckets, so a `A` view swap or a filter never
/// needs a re-read.
fn build_meta(all: &[&[PathBuf]]) -> std::collections::HashMap<PathBuf, WsMeta> {
    let mut map = std::collections::HashMap::new();
    for list in all {
        for p in *list {
            map.entry(p.clone())
                .or_insert_with(|| read_workspace_meta(p));
        }
    }
    map
}

/// Case-insensitive fuzzy subsequence test: every non-space char of
/// `needle` must appear in `hay` in order. Empty needle always
/// matches. Keeps the picker's recency order (we filter, we don't
/// re-rank) so rows don't jump around as you type.
fn fuzzy_match(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hay = hay.to_lowercase();
    let needle = needle.to_lowercase();
    let mut hc = hay.chars();
    'outer: for nc in needle.chars() {
        if nc == ' ' {
            continue;
        }
        for c in hc.by_ref() {
            if c == nc {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

/// Indices into `workspaces` that pass the active tag + text filters,
/// in original (recency) order. A `None` tag filter and empty query
/// pass everything.
fn compute_visible(
    workspaces: &[PathBuf],
    meta: &std::collections::HashMap<PathBuf, WsMeta>,
    tag_filter: Option<&str>,
    query: Option<&str>,
) -> Vec<usize> {
    (0..workspaces.len())
        .filter(|&i| {
            let m = meta.get(&workspaces[i]);
            let tag_ok = match tag_filter {
                None => true,
                Some(t) => m.is_some_and(|m| m.tags.iter().any(|x| x == t)),
            };
            if !tag_ok {
                return false;
            }
            match query {
                None | Some("") => true,
                Some(q) => {
                    let name = m.map(|m| m.name.as_str()).unwrap_or("");
                    let path = workspaces[i].to_string_lossy();
                    let tags = m.map(|m| m.tags.join(" ")).unwrap_or_default();
                    let hay = format!("{name} {path} {tags}");
                    fuzzy_match(&hay, q)
                }
            }
        })
        .collect()
}

/// The distinct tags present across the shown workspaces, ordered by
/// frequency (desc) then alphabetically — the order `f` cycles.
fn distinct_tags(
    workspaces: &[PathBuf],
    meta: &std::collections::HashMap<PathBuf, WsMeta>,
) -> Vec<String> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for p in workspaces {
        if let Some(m) = meta.get(p) {
            for t in &m.tags {
                *counts.entry(t.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut tags: Vec<(String, usize)> = counts.into_iter().collect();
    tags.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    tags.into_iter().map(|(t, _)| t).collect()
}

/// Advance the tag filter through: none → first → … → last → none.
/// Skips a stale filter (a tag no longer present) by restarting.
fn cycle_tag_filter(current: Option<&str>, tags: &[String]) -> Option<String> {
    if tags.is_empty() {
        return None;
    }
    match current {
        None => Some(tags[0].clone()),
        Some(cur) => match tags.iter().position(|t| t == cur) {
            Some(i) if i + 1 < tags.len() => Some(tags[i + 1].clone()),
            _ => None, // past the end (or stale) → back to unfiltered
        },
    }
}

/// Run the scaffold at `dir` for a fresh workspace. The display
/// name is the directory's basename; multiplexer comes from the
/// machine default (or tmux). Returns the new workspace file path
/// on success.
fn perform_scaffold_at(dir: &std::path::Path) -> anyhow::Result<PathBuf> {
    let display_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();
    let mpx = crate::config::current_default_multiplexer()
        .ok()
        .flatten()
        .unwrap_or(crate::domain::Multiplexer::Tmux);
    let outcome = crate::scaffold::create_at(dir, &display_name, mpx, false, false)?;
    Ok(outcome.path().to_path_buf())
}

/// Probe the multiplexers once and return a map of workspace path
/// → number of live sessions that correspond to its declared
/// sessions. "Live" means an mpx session exists under the
/// workspace-scoped name (matches what the session-list TUI shows
/// with the green ● marker, minus Idle and Untracked).
///
/// Errors are swallowed as 0 — a workspace whose TOML is broken or
/// whose mpx is unreachable just gets a live count of 0.
fn compute_live_counts(workspaces: &[PathBuf]) -> std::collections::HashMap<PathBuf, usize> {
    use std::collections::{HashMap, HashSet};
    let mut counts: HashMap<PathBuf, usize> = HashMap::new();

    // Resolve each workspace (name, mpx, session names).
    let mut resolved: Vec<(PathBuf, String, crate::domain::Multiplexer, Vec<String>)> =
        Vec::with_capacity(workspaces.len());
    for p in workspaces {
        let Ok(ws) = crate::config::load(&crate::config::LoadOptions {
            workspace_path: Some(p.clone()),
            ..Default::default()
        }) else {
            counts.insert(p.clone(), 0);
            continue;
        };
        let names: Vec<String> = ws.sessions.iter().map(|s| s.name.clone()).collect();
        resolved.push((p.clone(), ws.name, ws.multiplexer, names));
    }

    // Probe each distinct mpx at most once — spawning tmux / zellij
    // is ~100ms, so collapsing across workspaces is worth it.
    let mut live_by_mpx: HashMap<crate::domain::Multiplexer, HashSet<String>> = HashMap::new();
    let unique_mpxs: HashSet<crate::domain::Multiplexer> =
        resolved.iter().map(|(_, _, m, _)| *m).collect();
    for mpx in unique_mpxs {
        let mux: Option<Box<dyn crate::mux::Multiplexer>> = match mpx {
            crate::domain::Multiplexer::Tmux => Some(Box::new(crate::mux::TmuxAdapter::new())),
            crate::domain::Multiplexer::Zellij => Some(Box::new(crate::mux::ZellijAdapter::new())),
            crate::domain::Multiplexer::Wezterm => None,
        };
        if let Some(m) = mux {
            if let Ok(live) = m.list_sessions() {
                live_by_mpx.insert(mpx, live.into_iter().map(|s| s.name).collect());
            }
        }
    }

    // Count per workspace.
    for (path, ws_name, mpx, session_names) in &resolved {
        let Some(live) = live_by_mpx.get(mpx) else {
            counts.insert(path.clone(), 0);
            continue;
        };
        let n = session_names
            .iter()
            .filter(|sn| live.contains(&crate::mux::workspace_session_name(ws_name, sn)))
            .count();
        counts.insert(path.clone(), n);
    }
    counts
}

fn clamp_selection(workspaces: &[PathBuf], state: &mut ListState) {
    let total = workspaces.len() + 1; // + live-sessions sentinel
    let sel = state.selected().unwrap_or(0);
    if total == 0 {
        state.select(None);
    } else {
        state.select(Some(sel.min(total - 1)));
    }
}

/// Up-to-`max` dim `#tag` chips, with a `+N` overflow marker.
fn tag_chip_spans(tags: &[String], max: usize) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let chip = Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM);
    for (i, t) in tags.iter().enumerate() {
        if i >= max {
            out.push(Span::styled(
                format!(" +{}", tags.len() - max),
                Style::default().add_modifier(Modifier::DIM),
            ));
            break;
        }
        out.push(Span::raw(" "));
        out.push(Span::styled(format!("#{t}"), chip));
    }
    out
}

fn render(
    frame: &mut Frame<'_>,
    state: &mut ListState,
    search: &mut Option<crate::tui::find::SearchState>,
    ctx: RenderCtx,
) {
    let archived_view = ctx.view == PickerView::Archived;
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // spacer/hint (or filter input)
            Constraint::Min(0),    // list
            Constraint::Length(1), // footer line 1
            Constraint::Length(1), // footer line 2
        ])
        .split(area);

    // Title — appends a filter indicator when a tag filter is active.
    let title_text = match ctx.tag_filter {
        Some(t) => format!(" portagenty  ·  #{t} "),
        None if archived_view => " portagenty  ·  archived workspaces ".to_string(),
        None => " portagenty  ·  pick a workspace ".to_string(),
    };
    let title = Paragraph::new(title_text).style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(title, chunks[0]);

    // Hint / filter row.
    let dim = Style::default().add_modifier(Modifier::DIM);
    let cyan = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    if let Some(q) = ctx.text_filter {
        // Incremental `/` filter input line.
        let line = Line::from(vec![
            Span::styled(" /", cyan),
            Span::styled(q.to_string(), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("▏", cyan.add_modifier(Modifier::SLOW_BLINK)),
            Span::raw("   "),
            Span::styled(
                format!("matched {} of {}", ctx.visible.len(), ctx.workspaces.len()),
                dim,
            ),
            Span::raw("  ·  "),
            Span::styled("Esc to clear", dim),
        ]);
        frame.render_widget(Paragraph::new(line), chunks[1]);
    } else if let Some(t) = ctx.tag_filter {
        let line = Line::from(vec![
            Span::raw(" filter: "),
            Span::styled(format!("#{t}"), cyan),
            Span::styled(format!("  ({} shown)", ctx.visible.len()), dim),
            Span::raw("   "),
            Span::styled("f cycles · Esc clears", dim),
        ]);
        frame.render_widget(Paragraph::new(line), chunks[1]);
    } else {
        let hint_text = if archived_view {
            " Archived workspaces — hidden from the main list. a unarchives; A returns to the main list. "
        } else {
            " Pick a workspace, or browse live sessions.  / filter · f tag-filter · t tag · a archive "
        };
        frame.render_widget(Paragraph::new(hint_text).style(dim), chunks[1]);
    }

    let row_width = chunks[2].width as usize;
    let mut items: Vec<ListItem> = Vec::with_capacity(ctx.visible.len() + 1);
    for &idx in ctx.visible {
        let path = &ctx.workspaces[idx];
        let m = ctx.meta.get(path);
        let label = m
            .map(|m| m.name.clone())
            .unwrap_or_else(|| read_workspace_meta(path).name);
        let tags: &[String] = m.map(|m| m.tags.as_slice()).unwrap_or(&[]);
        let dir = path
            .parent()
            .map(|p| compact_path(&p.display().to_string()))
            .unwrap_or_default();
        let relative = crate::state::relative_time(crate::state::last_launch_for_workspace(path));
        let live_n = ctx.live_counts.get(path).copied().unwrap_or(0);
        let live_badge: Option<Span<'static>> = if live_n > 0 {
            Some(Span::styled(
                format!("● {live_n} live"),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            None
        };

        if row_width >= 70 {
            let name_budget = label.chars().count().min(22);
            let used = 6 + name_budget + 3 + 12 + 10 + 2;
            let path_budget = row_width.saturating_sub(used).clamp(10, 50);
            let mut spans = vec![
                Span::raw(" "),
                Span::styled("●", Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(label.clone(), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("   "),
                Span::styled(truncate_middle(&dir, path_budget), dim),
                Span::raw("   "),
                Span::styled(relative, dim),
            ];
            if let Some(b) = live_badge {
                spans.push(Span::raw("   "));
                spans.push(b);
            }
            // Tag chips ride the trailing flex region so the fixed
            // columns above never shift.
            spans.extend(tag_chip_spans(tags, 3));
            items.push(ListItem::new(Line::from(spans)));
        } else {
            let path_budget = row_width.saturating_sub(6).max(10);
            let mut line1 = vec![
                Span::raw(" "),
                Span::styled("●", Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(label.clone(), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(relative, dim),
            ];
            if let Some(b) = live_badge {
                line1.push(Span::raw("  "));
                line1.push(b);
            }
            line1.extend(tag_chip_spans(tags, 2));
            items.push(ListItem::new(vec![
                Line::from(line1),
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(truncate_middle(&dir, path_budget), dim),
                ]),
            ]));
        }
    }
    // Empty-state line when nothing is visible (filtered or empty
    // archived view). The unfiltered active view always has the
    // sentinel, so it's never empty.
    if items.is_empty() {
        let msg = if ctx.text_filter.is_some() || ctx.tag_filter.is_some() {
            "  (no matches — Esc to clear the filter)"
        } else if archived_view {
            "  (no archived workspaces — press A to go back)"
        } else {
            ""
        };
        if !msg.is_empty() {
            items.push(ListItem::new(Line::from(vec![Span::styled(msg, dim)])));
        }
    }
    // Sentinel row: live browse (unfiltered active view only).
    if ctx.has_sentinel {
        items.push(ListItem::new(Line::from(vec![
            Span::raw(" "),
            Span::styled("…", dim),
            Span::raw("  "),
            Span::styled(
                "live sessions on this machine",
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::DIM),
            ),
            Span::raw("   "),
            Span::styled("(no workspace — just attach to what's running)", dim),
        ])));
    }

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, chunks[2], state);

    if let Some(s) = ctx.status {
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                s.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("(Esc dismisses)", dim),
        ]);
        frame.render_widget(Paragraph::new(line), chunks[3]);
        frame.render_widget(Paragraph::new(""), chunks[4]);
    } else {
        use crate::tui::footer::Entry;
        crate::tui::footer::render(
            frame,
            chunks[3],
            &[
                Entry::new("q", "quit"),
                Entry::new("?", "help"),
                Entry::new("Enter/l", "open"),
                Entry::new("/", "filter"),
                Entry::new("j/k", "nav"),
            ],
        );
        let sep = Style::default().fg(Color::DarkGray);
        let key = cyan;
        let lbl = dim;
        let actions: &[(&str, &str)] = if archived_view {
            &[
                ("a", "unarchive  "),
                ("A", "back  "),
                ("d", "unreg  "),
                ("D", "delete  "),
                ("r", "reveal"),
            ]
        } else {
            &[
                ("n", "new  "),
                ("f", "tag  "),
                ("t", "edit-tags  "),
                ("a", "archive  "),
                ("A", "archived  "),
                ("d", "unreg  "),
                ("D", "delete  "),
                ("X", "kill  "),
                ("R", "rename"),
            ]
        };
        let mut spans: Vec<Span> = vec![Span::styled(" ─── ", sep)];
        for (k, l) in actions {
            spans.push(Span::styled(format!("{k} "), key));
            spans.push(Span::styled(*l, lbl));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), chunks[4]);
    }

    if let Some(p) = ctx.pending {
        match p {
            PickerPending::Rename { path, input } => {
                render_input_modal(frame, area, "Rename workspace", "name", path, input);
            }
            PickerPending::EditTags { path, input } => {
                render_input_modal(
                    frame,
                    area,
                    "Edit tags (comma-separated)",
                    "tags",
                    path,
                    input,
                );
            }
            _ => {
                let (title, body) = picker_confirm_copy(p);
                crate::tui::confirm::render(frame, area, &title, &body);
            }
        }
    }

    if let Some(modal) = ctx.info {
        crate::tui::confirm::render_info(frame, area, &modal.title, modal.lines.clone());
    }

    if let Some(s) = search.as_mut() {
        crate::tui::find::render(frame, area, s);
    }

    if ctx.help_open {
        crate::tui::help::render_overlay(frame, area, crate::tui::help::HelpContext::Picker);
    }
}

fn picker_confirm_copy(p: &PickerPending) -> (String, String) {
    match p {
        PickerPending::Unregister(path) => (
            "Unregister workspace".into(),
            format!(
                "Drop this workspace from the global picker index? \
                 The file {} stays on disk; you can re-register with \
                 `pa init` or by running `pa onboard` here.",
                path.display(),
            ),
        ),
        PickerPending::DeleteFile(path) => (
            "Delete workspace file".into(),
            format!(
                "DELETE THE FILE {} from disk and remove it from the global \
                 picker index? This is destructive — the workspace TOML \
                 cannot be recovered unless you have a backup or git history.",
                path.display(),
            ),
        ),
        PickerPending::ScaffoldAt(path) => {
            let stem = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("workspace");
            (
                "Scaffold new workspace".into(),
                format!(
                    "Create a new workspace named '{stem}' at {}? \
                     A `{stem}.portagenty.toml` will be written there with \
                     a starter shell session, registered globally, and opened \
                     in the session TUI immediately.",
                    path.display(),
                ),
            )
        }
        PickerPending::KillAllSessions {
            ws_display_name,
            mpx: _,
            targets,
        } => {
            let n = targets.len();
            let tracked = targets.iter().filter(|t| t.tracked).count();
            let untracked = n - tracked;
            let mix = match (tracked, untracked) {
                (_, 0) => format!(" ({tracked} tracked)"),
                (0, _) => format!(" ({untracked} untracked)"),
                _ => format!(" ({tracked} tracked + {untracked} untracked)"),
            };
            let list = targets
                .iter()
                .map(|t| {
                    let tag = if t.tracked { "tracked" } else { "untracked" };
                    format!("  · {} ({tag})", t.display)
                })
                .collect::<Vec<_>>()
                .join("\n");
            (
                "Kill all live sessions".into(),
                format!(
                    "Kill {n} live session{plural} under {ws_display_name:?}?{mix}\n\
                     \n{list}\n\
                     \n\
                     Sessions die immediately — running state is lost.",
                    plural = if n == 1 { "" } else { "s" },
                ),
            )
        }
        // Rename + EditTags are drawn by their own input modal; never
        // routed through the y/N confirm path. Arms kept exhaustive.
        PickerPending::Rename { .. } | PickerPending::EditTags { .. } => {
            (String::new(), String::new())
        }
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

/// Centered single-field text-input modal. Used by both the rename
/// (`field_label = "name"`) and tag-edit (`field_label = "tags"`)
/// flows. Caller handles key routing (Enter = commit, Esc = cancel);
/// this just draws the box with `title`, the workspace's file stem,
/// the labeled input, and a help line.
fn render_input_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    field_label: &str,
    path: &std::path::Path,
    input: &str,
) {
    let w = area.width;
    let h = area.height;
    let overlay_w = (w.saturating_sub(4)).clamp(40, 72);
    let overlay_h: u16 = 7;
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

    let file_label = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("workspace");

    let block = Block::default()
        .title(format!(" {title} "))
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let lines = vec![
        Line::from(vec![
            Span::styled("  file: ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                file_label.to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  {field_label}: "),
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
        Line::from(""),
        Line::from(Span::styled(
            "  Enter to save · Esc to cancel · Ctrl+U clears",
            Style::default().add_modifier(Modifier::DIM),
        )),
    ];

    frame.render_widget(Paragraph::new(lines).block(block), region);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_of(pairs: &[(&str, &[&str])]) -> std::collections::HashMap<PathBuf, WsMeta> {
        pairs
            .iter()
            .map(|(name, tags)| {
                (
                    PathBuf::from(format!("/ws/{name}.portagenty.toml")),
                    WsMeta {
                        name: name.to_string(),
                        tags: tags.iter().map(|t| t.to_string()).collect(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn fuzzy_match_is_case_insensitive_subsequence() {
        assert!(fuzzy_match("retake-studio", "rst"));
        assert!(fuzzy_match("Retake-Studio", "RETAKE"));
        assert!(fuzzy_match("retake-studio", "")); // empty always matches
        assert!(!fuzzy_match("retake-studio", "xyz"));
        assert!(!fuzzy_match("abc", "abcd")); // needle longer / not present
        assert!(fuzzy_match("a b c", "abc")); // spaces in needle ignored
    }

    #[test]
    fn compute_visible_filters_by_tag_and_query() {
        let ws: Vec<PathBuf> = ["alpha", "beta", "gamma"]
            .iter()
            .map(|n| PathBuf::from(format!("/ws/{n}.portagenty.toml")))
            .collect();
        let meta = meta_of(&[
            ("alpha", &["rust", "tui"]),
            ("beta", &["rust"]),
            ("gamma", &["python"]),
        ]);
        // No filters → everything.
        assert_eq!(compute_visible(&ws, &meta, None, None).len(), 3);
        // Tag filter #rust → alpha + beta (indices 0,1).
        assert_eq!(compute_visible(&ws, &meta, Some("rust"), None), vec![0, 1]);
        // Query "gam" → gamma only (index 2).
        assert_eq!(compute_visible(&ws, &meta, None, Some("gam")), vec![2]);
        // Both: #rust AND "alp" → alpha only.
        assert_eq!(
            compute_visible(&ws, &meta, Some("rust"), Some("alp")),
            vec![0]
        );
        // Query matches the path too.
        assert_eq!(compute_visible(&ws, &meta, None, Some("beta")), vec![1]);
    }

    #[test]
    fn distinct_tags_ordered_by_frequency_then_alpha() {
        let ws: Vec<PathBuf> = ["a", "b", "c"]
            .iter()
            .map(|n| PathBuf::from(format!("/ws/{n}.portagenty.toml")))
            .collect();
        let meta = meta_of(&[
            ("a", &["rust", "zebra"]),
            ("b", &["rust", "agentic"]),
            ("c", &["rust"]),
        ]);
        // rust (3×) first; then agentic, zebra (1× each) alphabetically.
        assert_eq!(distinct_tags(&ws, &meta), vec!["rust", "agentic", "zebra"]);
    }

    #[test]
    fn cycle_tag_filter_walks_none_to_last_to_none() {
        let tags = vec!["rust".to_string(), "tui".to_string()];
        assert_eq!(cycle_tag_filter(None, &tags).as_deref(), Some("rust"));
        assert_eq!(
            cycle_tag_filter(Some("rust"), &tags).as_deref(),
            Some("tui")
        );
        assert_eq!(cycle_tag_filter(Some("tui"), &tags), None); // past end → clear
        assert_eq!(cycle_tag_filter(Some("stale"), &tags), None); // stale → clear
        assert_eq!(cycle_tag_filter(None, &[]), None); // no tags → stays none
    }

    #[test]
    fn selected_ws_index_maps_through_visible_and_sentinel() {
        let visible = vec![2usize, 5, 7];
        let mut state = ListState::default();
        state.select(Some(1));
        assert_eq!(selected_ws_index(&visible, &state, true), Some(5));
        // Sentinel row (index == visible.len()) → None.
        state.select(Some(3));
        assert_eq!(selected_ws_index(&visible, &state, true), None);
        // Without a sentinel, index 3 is out of range → None.
        assert_eq!(selected_ws_index(&visible, &state, false), None);
    }

    #[test]
    fn edit_text_input_handles_readline_keys() {
        let mut s = String::from("rust, tui");
        edit_text_input(&mut s, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(s, "rust, tu");
        edit_text_input(&mut s, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(s, "rust, ");
        edit_text_input(&mut s, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(s, "");
        edit_text_input(&mut s, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(s, "x");
        // Other Ctrl combos are ignored (don't insert a literal char).
        edit_text_input(&mut s, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(s, "x");
    }
}
