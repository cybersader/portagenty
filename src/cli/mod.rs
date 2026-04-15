//! CLI parsing and one-shot subcommands. The bare `pa` invocation drops into
//! the TUI; subcommands here are scriptable equivalents.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use crate::config::{load, LoadOptions};
use crate::domain::{Multiplexer as MpxEnum, Session, Workspace};
use crate::mux::{Multiplexer, TmuxAdapter};

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
    /// Attach to (or create-and-attach) a session by name, without
    /// entering the TUI.
    Launch {
        /// Session name as declared in the workspace.
        session: String,

        /// Explicit path to a `*.portagenty.toml` file. When omitted,
        /// portagenty walks up from the current directory.
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<PathBuf>,

        /// Print what would be launched instead of actually running
        /// the multiplexer. Useful for scripts + tests.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Print the currently-resolved workspace (name, multiplexer,
    /// sessions) to stdout.
    List {
        /// Explicit path to a `*.portagenty.toml` file. When omitted,
        /// portagenty walks up from the current directory.
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<PathBuf>,
    },
}

/// Resolve the session the user named in the current (or explicit)
/// workspace. Returns the Session clone plus the owning Workspace.
fn resolve(session_name: &str, workspace: Option<&PathBuf>) -> Result<(Session, Workspace)> {
    let ws = load(&LoadOptions {
        workspace_path: workspace.cloned(),
        ..Default::default()
    })?;

    let session = ws
        .sessions
        .iter()
        .find(|s| s.name == session_name)
        .cloned()
        .ok_or_else(|| {
            let available: Vec<&str> = ws.sessions.iter().map(|s| s.name.as_str()).collect();
            if available.is_empty() {
                anyhow!(
                    "workspace {:?} has no sessions; cannot launch {session_name:?}",
                    ws.name
                )
            } else {
                anyhow!(
                    "no session named {session_name:?} in workspace {:?}. available: {}",
                    ws.name,
                    available.join(", ")
                )
            }
        })?;
    Ok((session, ws))
}

/// Build a concrete [`Multiplexer`] from the workspace's pinned enum.
/// v1 ships only tmux; the other variants return a clear "not yet
/// implemented" error so a workspace can be authored ahead of its
/// adapter landing in v1.x.
fn build_mux(kind: MpxEnum) -> Result<Box<dyn Multiplexer>> {
    match kind {
        MpxEnum::Tmux => Ok(Box::new(TmuxAdapter::new())),
        MpxEnum::Zellij => Err(anyhow!(
            "the zellij multiplexer adapter is not implemented yet (v1.x)"
        )),
        MpxEnum::Wezterm => Err(anyhow!(
            "the wezterm multiplexer adapter is not implemented yet (v1.x)"
        )),
    }
}

pub fn launch(session: &str, workspace: Option<&PathBuf>, dry_run: bool) -> Result<()> {
    let (sess, ws) = resolve(session, workspace)?;

    if dry_run {
        let out = io::stdout();
        let mut out = out.lock();
        writeln!(out, "would launch {:?} via {:?}", sess.name, ws.multiplexer)?;
        writeln!(out, "  cwd:     {}", sess.cwd.display())?;
        writeln!(out, "  command: {}", sess.command)?;
        return Ok(());
    }

    // Record the launch BEFORE attaching — attach blocks until the
    // user detaches from the mpx, so recording after could lose the
    // entry if the process is killed mid-session.
    if let Some(path) = &ws.file_path {
        let _ = crate::state::record_launch(path, &sess.name);
    }

    let mux = build_mux(ws.multiplexer)?;
    mux.create_and_attach(&sess)
        .with_context(|| format!("launching session {:?}", sess.name))
}

pub fn list(workspace: Option<&PathBuf>) -> Result<()> {
    let ws = load(&LoadOptions {
        workspace_path: workspace.cloned(),
        ..Default::default()
    })?;

    let out = io::stdout();
    let mut out = out.lock();
    writeln!(out, "workspace: {}", ws.name)?;
    if let Some(path) = &ws.file_path {
        writeln!(out, "file:      {}", path.display())?;
    }
    writeln!(out, "mpx:       {:?}", ws.multiplexer)?;
    writeln!(out, "projects:  {}", ws.projects.len())?;
    for p in &ws.projects {
        writeln!(out, "  - {}", p.display())?;
    }
    writeln!(out, "sessions:  {}", ws.sessions.len())?;
    for s in &ws.sessions {
        writeln!(
            out,
            "  - {}  (cwd: {})  {}",
            s.name,
            s.cwd.display(),
            s.command
        )?;
    }
    Ok(())
}
