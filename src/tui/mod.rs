//! ratatui app: workspace tree + session list, vim-flavored keybindings,
//! responsive layout for Termux/small-screen use. See `DESIGN.md` §10.

pub mod app;
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
    // Two-phase load: try to find a workspace. If walk-up fails AND
    // we're in an interactive shell AND the user hasn't seen
    // onboarding yet, offer the first-run wizard instead of just
    // erroring. Scripted / piped / CI runs bypass the wizard and
    // get the original error.
    let workspace = match load(&LoadOptions::default()) {
        Ok(w) => w,
        Err(e) => {
            if crate::onboarding::is_interactive() && !crate::onboarding::has_onboarded() {
                use crate::onboarding::OnboardOutcome;
                match crate::onboarding::run_wizard(false)? {
                    OnboardOutcome::Scaffolded { .. } => {
                        // File was just created in cwd — retry load.
                        load(&LoadOptions::default())?
                    }
                    OnboardOutcome::ShowedDocs | OnboardOutcome::Skipped => {
                        return Ok(());
                    }
                }
            } else {
                return Err(e);
            }
        }
    };
    let workspace_file = workspace.file_path.clone();
    let mpx_kind = workspace.multiplexer;
    let mux: Box<dyn crate::mux::Multiplexer> = match workspace.multiplexer {
        crate::domain::Multiplexer::Tmux => Box::new(TmuxAdapter::new()),
        crate::domain::Multiplexer::Zellij => {
            // zellij refuses nested sessions, and erroring out *after*
            // the TUI tears down leaves the message liable to scroll
            // off-screen. Catch it up front so the user sees a clean
            // actionable error on the shell they launched `pa` from.
            if crate::mux::ZellijAdapter::is_inside_zellij() {
                let cur =
                    std::env::var("ZELLIJ_SESSION_NAME").unwrap_or_else(|_| "<unknown>".into());
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

    // Best-effort live-session snapshot. A failure here shouldn't
    // block the TUI — the user might not have tmux / zellij running
    // yet, and we can still show workspace sessions.
    let live = mux.list_sessions().unwrap_or_default();

    let app = App::new(workspace, mux, live);

    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();

    // TUI Enter defaults to takeover — "I'm here now, this is the
    // main client." Matches the cross-device ergonomic DESIGN sketch
    // has always implied. A future key could offer shared-attach if
    // there's demand.
    let mode = crate::mux::AttachMode::Takeover;
    let launch_result = match result? {
        (AppOutcome::Quit, _) => return Ok(()),
        (AppOutcome::Launch(LaunchKind::Create { session }), mux) => {
            if let Some(path) = &workspace_file {
                let _ = crate::state::record_launch(path, &session.name);
            }
            print_launch_banner(mpx_kind, &session.name);
            mux.create_and_attach(&session, mode)
        }
        (AppOutcome::Launch(LaunchKind::Attach { mpx_name }), mux) => {
            if let Some(path) = &workspace_file {
                let _ = crate::state::record_launch(path, &mpx_name);
            }
            print_launch_banner(mpx_kind, &mpx_name);
            mux.attach(&mpx_name, mode)
        }
    };
    // A post-TUI error on a freshly-restored terminal can be easy to
    // miss — the next shell prompt scrolls it. Emit a loud header so
    // the user sees that something actually went wrong.
    if let Err(e) = &launch_result {
        eprintln!();
        eprintln!("  pa: couldn't launch the selected session.");
        eprintln!("  {e:#}");
    }
    launch_result
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
