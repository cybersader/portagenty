//! portagenty: portable, terminal-native launcher for agent workspaces.
//!
//! See `DESIGN.md` for the architectural deep-dive and `ROADMAP.md` for the
//! v1 / v1.x sequence. This crate is in early bootstrap: most modules are
//! skeleton placeholders and will be filled in subsequent chunks.

pub mod cli;
pub mod clipboard;
pub mod config;
pub mod domain;
pub mod export;
pub mod find;
pub mod mux;
pub mod onboarding;
pub mod scaffold;
pub mod snippets;
pub mod state;
pub mod tui;
pub mod workspace_edit;

use cli::{Cli, Command};

/// Entry point shared by the binary and integration tests. Dispatches the
/// parsed CLI into either the TUI (default) or a one-shot subcommand.
pub fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        None => tui::run(cli.path.as_deref()),
        Some(Command::Launch {
            session,
            workspace,
            dry_run,
            shared,
            resume,
        }) => cli::launch(&session, workspace.as_ref(), dry_run, shared, resume),
        Some(Command::Claim {
            session,
            workspace,
            dry_run,
            resume,
        }) => cli::claim(session.as_deref(), workspace.as_ref(), dry_run, resume),
        Some(Command::List { workspace }) => cli::list(workspace.as_ref()),
        Some(Command::Export {
            workspace,
            format,
            output,
        }) => cli::export(workspace.as_ref(), format, output.as_ref()),
        Some(Command::Init { name, mpx, force }) => cli::init(name, mpx, force),
        Some(Command::Snippets(cmd)) => cli::snippets(cmd),
        Some(Command::Onboard) => cli::onboard(),
        Some(Command::Completions { shell }) => cli::completions(shell),
        Some(Command::Add {
            name,
            command,
            cwd,
            kind,
            workspace,
        }) => cli::add(&name, &command, cwd.as_deref(), kind, workspace.as_ref()),
        Some(Command::Rm { name, workspace }) => cli::rm(&name, workspace.as_ref()),
        Some(Command::Edit {
            name,
            command,
            cwd,
            kind,
            rename,
            env,
            unset_env,
            workspace,
        }) => cli::edit(
            &name,
            command.as_deref(),
            cwd.as_deref(),
            kind,
            rename.as_deref(),
            &env,
            &unset_env,
            workspace.as_ref(),
        ),
    }
}
