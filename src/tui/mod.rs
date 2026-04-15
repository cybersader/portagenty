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
    let workspace = load(&LoadOptions::default())?;
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

    match result? {
        (AppOutcome::Quit, _) => Ok(()),
        (AppOutcome::Launch(LaunchKind::Create { session }), mux) => {
            if let Some(path) = &workspace_file {
                let _ = crate::state::record_launch(path, &session.name);
            }
            mux.create_and_attach(&session)
        }
        (AppOutcome::Launch(LaunchKind::Attach { mpx_name }), mux) => {
            if let Some(path) = &workspace_file {
                let _ = crate::state::record_launch(path, &mpx_name);
            }
            mux.attach(&mpx_name)
        }
    }
}
