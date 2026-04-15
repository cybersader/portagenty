//! ratatui app: workspace tree + session list, vim-flavored keybindings,
//! responsive layout for Termux/small-screen use. See `DESIGN.md` §10.

pub mod app;

pub use app::App;

use anyhow::Result;

use crate::config::{load, LoadOptions};
use crate::mux::TmuxAdapter;

/// Entry point for the bare `pa` invocation. Loads the current
/// workspace, spins up the TUI, and runs until the user quits.
pub fn run() -> Result<()> {
    let workspace = load(&LoadOptions::default())?;
    let mux = Box::new(TmuxAdapter::new());

    let mut terminal = ratatui::init();
    let result = App::new(workspace, mux).run(&mut terminal);
    ratatui::restore();
    result
}
