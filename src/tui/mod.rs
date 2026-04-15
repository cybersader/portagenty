//! ratatui app: workspace tree + session list, vim-flavored keybindings,
//! responsive layout for Termux/small-screen use. See `DESIGN.md` §10.

pub mod app;

pub use app::{Action, App, AppOutcome};

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
        (AppOutcome::Launch(session), mux) => mux.create_and_attach(&session),
    }
}
