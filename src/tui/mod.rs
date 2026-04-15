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
    let mux: Box<dyn crate::mux::Multiplexer> = match workspace.multiplexer {
        crate::domain::Multiplexer::Tmux => Box::new(TmuxAdapter::new()),
        crate::domain::Multiplexer::Zellij => Box::new(crate::mux::ZellijAdapter::new()),
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
    match result? {
        (AppOutcome::Quit, _) => Ok(()),
        (AppOutcome::Launch(LaunchKind::Create { session }), mux) => {
            if let Some(path) = &workspace_file {
                let _ = crate::state::record_launch(path, &session.name);
            }
            mux.create_and_attach(&session, mode)
        }
        (AppOutcome::Launch(LaunchKind::Attach { mpx_name }), mux) => {
            if let Some(path) = &workspace_file {
                let _ = crate::state::record_launch(path, &mpx_name);
            }
            mux.attach(&mpx_name, mode)
        }
    }
}
