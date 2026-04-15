//! ratatui app: workspace tree + session list, vim-flavored keybindings,
//! responsive layout for Termux/small-screen use. See `DESIGN.md` §10.

pub mod app;
pub mod picker;
pub mod view;

pub use app::{Action, App, AppOutcome, LaunchKind};
pub use view::{build_rows, SessionRow, SessionState};

use anyhow::Result;

use crate::config::{load, LoadOptions};
use crate::mux::TmuxAdapter;

/// Entry point for the bare `pa` invocation. Loads the current
/// workspace + live mpx sessions, runs the TUI, and — if the user
/// picked a row — restores the terminal and hands off to the mpx.
pub fn run() -> Result<()> {
    // See DESIGN.md §12 for the full entry-point contract. Key
    // invariant: the workspace picker is the *home screen*. Esc from
    // the session TUI always returns here, regardless of whether the
    // user entered via walk-up, wizard-scaffold, or the picker
    // itself. There is exactly one back-stack; pa is never ambiguous
    // about "what does Esc do."

    // First-run wizard short-circuits before the TUI loop, since
    // showing the picker with zero workspaces on a brand-new machine
    // would just bounce straight to onboarding anyway.
    if load(&LoadOptions::default()).is_err()
        && crate::onboarding::is_interactive()
        && !crate::onboarding::has_onboarded()
    {
        use crate::onboarding::OnboardOutcome;
        match crate::onboarding::run_wizard(false)? {
            OnboardOutcome::ShowedDocs | OnboardOutcome::Skipped => return Ok(()),
            OnboardOutcome::Scaffolded { .. } => {
                // Fall through to the TUI loop with the new workspace
                // pre-selected. Walk-up will pick it up now.
            }
        }
    }

    // Non-interactive (piped, CI, cron) with no walkable workspace
    // and no onboarding: nothing useful to show, exit cleanly with
    // the original error.
    if !crate::onboarding::is_interactive() && load(&LoadOptions::default()).is_err() {
        return Err(anyhow::anyhow!(
            "no *.portagenty.toml found walking up from {}",
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".into())
        ));
    }

    // Main loop: session TUI ↔ picker, sharing one ratatui session.
    // On the first iteration, if walk-up finds a workspace, show its
    // session TUI directly. On subsequent iterations (after Esc),
    // always show the picker.
    let mut terminal = ratatui::init();
    let mut first_iteration = true;
    loop {
        let ws = if first_iteration {
            first_iteration = false;
            load(&LoadOptions::default()).ok()
        } else {
            None // Back was pressed — always show picker from here on
        };

        let ws = match ws {
            Some(w) => w,
            None => match show_picker(&mut terminal) {
                Ok(Some(w)) => w,
                Ok(None) => {
                    ratatui::restore();
                    return Ok(());
                }
                Err(e) => {
                    ratatui::restore();
                    return Err(e);
                }
            },
        };

        match run_session_tui(&mut terminal, ws) {
            Ok(SessionRunOutcome::Back) => continue,
            Ok(SessionRunOutcome::Quit) => {
                ratatui::restore();
                return Ok(());
            }
            Ok(SessionRunOutcome::Launched(r)) => {
                ratatui::restore();
                return finalize_launch(r);
            }
            Err(e) => {
                ratatui::restore();
                return Err(e);
            }
        }
    }
}

/// Run the workspace picker and return the selected workspace, or
/// `None` if the user quit. `Err` only for unexpected IO errors.
fn show_picker(
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<Option<crate::domain::Workspace>> {
    let mut registered = crate::config::list_registered_workspaces().unwrap_or_default();
    // Recency sort: workspaces with a recorded launch come first,
    // most-recent at the top; workspaces never launched fall to the
    // bottom in alphabetical order. The "live sessions" sentinel is
    // added by the picker itself and always trails.
    registered.sort_by(|a, b| {
        let ra = crate::state::last_launch_for_workspace(a);
        let rb = crate::state::last_launch_for_workspace(b);
        match (ra, rb) {
            (Some(x), Some(y)) => y.cmp(&x), // more recent first
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.file_name().cmp(&b.file_name()),
        }
    });
    match picker::run(terminal, &registered)? {
        picker::PickerOutcome::Quit => Ok(None),
        picker::PickerOutcome::LiveBrowse => Ok(Some(synthetic_browse_workspace()?)),
        picker::PickerOutcome::Workspace(path) => {
            let opts = LoadOptions {
                workspace_path: Some(path),
                ..Default::default()
            };
            Ok(Some(load(&opts)?))
        }
    }
}

/// Outcome of a single session-TUI run, as seen by the outer driver.
enum SessionRunOutcome {
    /// Esc — caller should return to the picker (or quit if no picker).
    Back,
    /// q / Ctrl+C — caller should exit cleanly.
    Quit,
    /// User picked a session; deferred launch result for the caller.
    Launched(Result<()>),
}

/// Runs the session-list TUI against an already-initialized terminal.
/// Does *not* restore the terminal — caller owns init/restore so the
/// picker can share one ratatui session with the session list.
fn run_session_tui(
    terminal: &mut ratatui::DefaultTerminal,
    workspace: crate::domain::Workspace,
) -> Result<SessionRunOutcome> {
    let workspace_file = workspace.file_path.clone();
    let mpx_kind = workspace.multiplexer;
    let mux: Box<dyn crate::mux::Multiplexer> = match workspace.multiplexer {
        crate::domain::Multiplexer::Tmux => Box::new(TmuxAdapter::new()),
        crate::domain::Multiplexer::Zellij => {
            if crate::mux::ZellijAdapter::is_inside_zellij() {
                let cur =
                    std::env::var("ZELLIJ_SESSION_NAME").unwrap_or_else(|_| "<unknown>".into());
                // Restore before bailing so the user sees the message
                // on their shell, not over the ratatui buffer.
                ratatui::restore();
                anyhow::bail!(
                    "refusing to open the TUI: you're already inside zellij session {cur:?}.\n\
                     zellij can't attach to another session from within a client. Options:\n\
                       - Detach first (Ctrl+Q by default), then run `pa` again.\n\
                       - Or launch into the existing session directly: `zellij attach <name>`.\n\
                     Current live zellij sessions: run `zellij list-sessions -n -s` to see them."
                );
            }
            Box::new(crate::mux::ZellijAdapter::new())
        }
        crate::domain::Multiplexer::Wezterm => {
            anyhow::bail!("the wezterm multiplexer adapter is not implemented yet (v1.x)")
        }
    };

    let live = mux.list_sessions().unwrap_or_default();
    let app = App::new(workspace, mux, live);
    let (outcome, mux) = app.run(terminal)?;

    let mode = crate::mux::AttachMode::Takeover;
    Ok(match outcome {
        AppOutcome::Back => SessionRunOutcome::Back,
        AppOutcome::Quit => SessionRunOutcome::Quit,
        AppOutcome::Launch(LaunchKind::Create { session }) => {
            if let Some(path) = &workspace_file {
                let _ = crate::state::record_launch(path, &session.name);
            }
            // Restore terminal so the mpx takes a clean tty and the
            // pre-launch banner prints to the user's real shell.
            ratatui::restore();
            print_launch_banner(mpx_kind, &session.name);
            SessionRunOutcome::Launched(mux.create_and_attach(&session, mode))
        }
        AppOutcome::Launch(LaunchKind::Attach { mpx_name }) => {
            if let Some(path) = &workspace_file {
                let _ = crate::state::record_launch(path, &mpx_name);
            }
            ratatui::restore();
            print_launch_banner(mpx_kind, &mpx_name);
            SessionRunOutcome::Launched(mux.attach(&mpx_name, mode))
        }
    })
}

/// Emit the loud post-restore banner on launch failure so the error
/// survives the user's next shell prompt.
fn finalize_launch(launch: Result<()>) -> Result<()> {
    if let Err(e) = &launch {
        eprintln!();
        eprintln!("  pa: couldn't launch the selected session.");
        eprintln!("  {e:#}");
    }
    launch
}

/// Build a synthetic empty workspace so the TUI can render
/// live-multiplexer sessions even when no `*.portagenty.toml` is
/// reachable from the current directory. Picks the machine-default
/// multiplexer if set; otherwise prefers zellij if installed, else
/// tmux. Returns an error if neither mpx is installed — at that
/// point there's literally nothing to show.
fn synthetic_browse_workspace() -> Result<crate::domain::Workspace> {
    use crate::domain::Multiplexer;
    let mpx = crate::config::current_default_multiplexer()
        .ok()
        .flatten()
        .unwrap_or_else(|| {
            // No pinned default — probe PATH. Fall through to tmux
            // when neither is present; the mpx adapter will surface a
            // friendlier "not installed" error at list_sessions time.
            if bin_on_path("zellij") {
                Multiplexer::Zellij
            } else {
                Multiplexer::Tmux
            }
        });
    Ok(crate::domain::Workspace {
        name: "(no workspace — live sessions)".into(),
        file_path: None,
        multiplexer: mpx,
        projects: vec![],
        sessions: vec![],
    })
}

fn bin_on_path(bin: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|d| d.join(bin).is_file())
}

/// Print a one-line hand-off banner just before we replace the TUI
/// with the multiplexer client. Tells the user which session they're
/// entering and the mpx-specific detach chord they'll need to get
/// back out. Keeping this info local to `pa` (no keybind rebinding,
/// no config mutation) means we don't couple to any specific zellij
/// or tmux version's defaults — users with custom configs just ignore
/// the hint and use their own chord.
fn print_launch_banner(mpx: crate::domain::Multiplexer, session: &str) {
    let detach = match mpx {
        crate::domain::Multiplexer::Tmux => "Ctrl+B then d",
        crate::domain::Multiplexer::Zellij => "Ctrl+O then d",
        crate::domain::Multiplexer::Wezterm => "see wezterm docs",
    };
    let mpx_name = match mpx {
        crate::domain::Multiplexer::Tmux => "tmux",
        crate::domain::Multiplexer::Zellij => "zellij",
        crate::domain::Multiplexer::Wezterm => "wezterm",
    };
    eprintln!();
    eprintln!("  pa → {mpx_name} session \"{session}\"");
    eprintln!("        detach: {detach}  ·  re-attach: pa claim {session}");
    eprintln!();
}
