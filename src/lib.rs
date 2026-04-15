//! portagenty: portable, terminal-native launcher for agent workspaces.
//!
//! See `DESIGN.md` for the architectural deep-dive and `ROADMAP.md` for the
//! v1 / v1.x sequence. This crate is in early bootstrap: most modules are
//! skeleton placeholders and will be filled in subsequent chunks.

pub mod cli;
pub mod config;
pub mod domain;
pub mod mux;
pub mod state;
pub mod tui;

use cli::{Cli, Command};

/// Entry point shared by the binary and integration tests. Dispatches the
/// parsed CLI into either the TUI (default) or a one-shot subcommand.
pub fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        None => tui::run(),
        Some(Command::Launch {
            session,
            workspace,
            dry_run,
        }) => cli::launch(&session, workspace.as_ref(), dry_run),
        Some(Command::List { workspace }) => cli::list(workspace.as_ref()),
    }
}
