//! ratatui app: workspace tree + session list, vim-flavored keybindings,
//! responsive layout for Termux/small-screen use. See `DESIGN.md` §10.

pub mod app;
pub mod view;

pub use app::{Action, App, AppOutcome};
pub use view::{build_rows, SessionRow, SessionState};

use anyhow::Result;

use crate::config::{load, LoadOptions};
use crate::mux::TmuxAdapter;

/// Entry point for the bare `pa` invocation. Loads the current
/// workspace, runs the TUI, and — if the user picked a session —
/// restores the terminal and hands off to the multiplexer to attach.
pub fn run() -> Result<()> {
    let workspace = load(&LoadOptions::default())?;
    let mux = Box::new(TmuxAdapter::new());
    let app = App::new(workspace, mux);

    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();

    match result? {
        (AppOutcome::Quit, _) => Ok(()),
        (AppOutcome::Launch(session), mux) => {
            // Record the launch before attaching; attach blocks until
            // the user detaches from the mpx, and if they kill the
            // whole process tree we'd lose the entry otherwise. The
            // record_launch is best-effort — a state-store failure
            // shouldn't block the user's launch.
            if let Some(path) = &workspace_file_from(&session) {
                let _ = crate::state::record_launch(path, &session.name);
            }
            mux.create_and_attach(&session)
        }
    }
}

/// We moved the Workspace into the App, which moved into `run`. The
/// workspace file path is no longer reachable through the Session
/// alone, so we'd need to either plumb it through AppOutcome or
/// re-derive it. Easiest: re-derive by walking up from the session's
/// cwd. Not perfect — if the workspace file is elsewhere — but for
/// v1 it's the simplest defensible choice. See DESIGN §4 for the
/// broader state-store contract.
fn workspace_file_from(session: &crate::domain::Session) -> Option<std::path::PathBuf> {
    crate::config::walk_up_from(&session.cwd)
}
