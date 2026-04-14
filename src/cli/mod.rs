//! CLI parsing and one-shot subcommands. The bare `pa` invocation drops into
//! the TUI; subcommands here are scriptable equivalents.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "pa",
    version,
    about = "Portable, terminal-native launcher for agent workspaces.",
    long_about = None,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Launch a session by `<workspace>/<session>` without entering the TUI.
    Launch {
        /// `<workspace>/<session>` identifier.
        target: String,
    },
    /// List known workspaces.
    List,
}

pub fn launch(_target: &str) -> anyhow::Result<()> {
    anyhow::bail!("`pa launch` is not implemented yet (planned in chunk E)")
}

pub fn list() -> anyhow::Result<()> {
    anyhow::bail!("`pa list` is not implemented yet (planned in chunk E)")
}
