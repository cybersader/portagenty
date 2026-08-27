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
pub(crate) mod process;
pub mod protocol;
pub mod scaffold;
pub mod snippets;
pub mod state;
pub mod supervision;
pub mod tui;
pub mod workspace_edit;

#[cfg(test)]
pub(crate) mod test_env;

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
            fresh,
            supervise,
            memory_high,
            memory_max,
            memory_swap_max,
            cpu_quota,
            tasks_max,
        }) => cli::launch(
            &session,
            workspace.as_ref(),
            dry_run,
            shared,
            resume,
            fresh,
            cli::LaunchSupervisionOptions {
                enabled: supervise,
                memory_high: memory_high.as_deref(),
                memory_max: memory_max.as_deref(),
                memory_swap_max: memory_swap_max.as_deref(),
                cpu_quota: cpu_quota.as_deref(),
                tasks_max: tasks_max.as_deref(),
            },
        ),
        Some(Command::Claim {
            session,
            workspace,
            dry_run,
            resume,
            fresh,
        }) => cli::claim(
            session.as_deref(),
            workspace.as_ref(),
            dry_run,
            resume,
            fresh,
        ),
        Some(Command::Resources(command)) => cli::resources(command),
        Some(Command::List { workspace }) => cli::list(workspace.as_ref()),
        Some(Command::Export {
            workspace,
            format,
            output,
        }) => cli::export(workspace.as_ref(), format, output.as_ref()),
        Some(Command::Init {
            name,
            mpx,
            force,
            with_agent_hooks,
        }) => cli::init(name, mpx, force, with_agent_hooks),
        Some(Command::Snippets(cmd)) => cli::snippets(cmd),
        Some(Command::Onboard) => cli::onboard(),
        Some(Command::WorkloadAnchor { spec }) => {
            #[cfg(target_os = "linux")]
            {
                supervision::linux_systemd::run_workload_anchor(&spec)
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = spec;
                anyhow::bail!("workload anchors are supported only on Linux")
            }
        }
        Some(Command::Completions { shell }) => cli::completions(shell),
        Some(Command::Add {
            name,
            command,
            cwd,
            kind,
            description,
            workspace,
        }) => cli::add(
            &name,
            &command,
            cwd.as_deref(),
            kind,
            description.as_deref(),
            workspace.as_ref(),
        ),
        Some(Command::Rm { name, workspace }) => cli::rm(&name, workspace.as_ref()),
        Some(Command::Edit {
            name,
            command,
            cwd,
            kind,
            rename,
            description,
            env,
            unset_env,
            workspace,
        }) => cli::edit(
            &name,
            command.as_deref(),
            cwd.as_deref(),
            kind,
            rename.as_deref(),
            description.as_deref(),
            &env,
            &unset_env,
            workspace.as_ref(),
        ),
        Some(Command::Open { url }) => cli::open_url(&url),
        Some(Command::Convos { workspace, args }) => cli::convos(workspace.as_ref(), &args),
        Some(Command::Protocol(cmd)) => match cmd {
            cli::ProtocolCommand::Terminals => cli::protocol_terminals(),
            cli::ProtocolCommand::Show { terminal } => cli::protocol_show(terminal.as_deref()),
            cli::ProtocolCommand::Install { terminal } => {
                cli::protocol_install(terminal.as_deref())
            }
            cli::ProtocolCommand::Uninstall => cli::protocol_uninstall(),
            cli::ProtocolCommand::Status => cli::protocol_status(),
        },
    }
}
