//! CLI parsing and one-shot subcommands. The bare `pa` invocation drops into
//! the TUI; subcommands here are scriptable equivalents.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use crate::config::{load, LoadOptions};
use crate::domain::{Multiplexer as MpxEnum, Session, Workspace};
use crate::mux::{AttachMode, Multiplexer, TmuxAdapter, ZellijAdapter};

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
    /// entering the TUI. Defaults to takeover mode — any other client
    /// attached to the same session gets bumped so the terminal size
    /// adjusts to this device. Pass `--shared` to keep the other
    /// client(s) attached.
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

        /// Don't detach other clients on attach. Multiple devices
        /// can watch the session at once; screen size is negotiated
        /// down to the smallest client.
        #[arg(long = "shared")]
        shared: bool,
    },
    /// "Make this device the main session." Short-form alias for
    /// `launch --takeover` that defaults the session name to the
    /// first session declared in the workspace.
    Claim {
        /// Optional session name. When omitted, the first session in
        /// the workspace is used. Errors if the workspace has no
        /// sessions.
        session: Option<String>,

        /// Explicit path to a `*.portagenty.toml` file.
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<PathBuf>,

        /// Print what would happen instead of invoking the multiplexer.
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
    /// Render the resolved workspace as a starter script (tmux) or
    /// layout (zellij). Useful for committing a per-machine launcher
    /// alongside the workspace TOML.
    Export {
        /// Explicit path to a `*.portagenty.toml` file.
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<PathBuf>,

        /// Output format. Defaults to whichever the workspace's
        /// `multiplexer` field resolves to.
        #[arg(long = "format", value_enum)]
        format: Option<ExportFormatArg>,

        /// Where to write the output. Default is stdout.
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ExportFormatArg {
    Tmux,
    Zellij,
}

impl From<ExportFormatArg> for crate::export::ExportFormat {
    fn from(a: ExportFormatArg) -> Self {
        match a {
            ExportFormatArg::Tmux => crate::export::ExportFormat::Tmux,
            ExportFormatArg::Zellij => crate::export::ExportFormat::Zellij,
        }
    }
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
        MpxEnum::Zellij => Ok(Box::new(ZellijAdapter::new())),
        MpxEnum::Wezterm => Err(anyhow!(
            "wezterm isn't supported by portagenty: its mux is built around the GUI \
             terminal's own window model, not the headless detach/reattach-over-SSH \
             pattern that powers `pa`'s cross-device workflow. Use tmux or zellij. \
             See ROADMAP v1.x for the rationale."
        )),
    }
}

pub fn launch(
    session: &str,
    workspace: Option<&PathBuf>,
    dry_run: bool,
    shared: bool,
) -> Result<()> {
    let (sess, ws) = resolve(session, workspace)?;
    let mode = if shared {
        AttachMode::Shared
    } else {
        AttachMode::Takeover
    };

    if dry_run {
        let out = io::stdout();
        let mut out = out.lock();
        writeln!(
            out,
            "would launch {:?} via {:?} ({})",
            sess.name,
            ws.multiplexer,
            attach_mode_label(mode),
        )?;
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
    mux.create_and_attach(&sess, mode)
        .with_context(|| format!("launching session {:?}", sess.name))
}

/// "Make this device the main session" — `pa claim`. Always uses
/// Takeover mode. Defaults the session name to the first one in the
/// workspace so the common case (only one agent-per-project) is a
/// single-arg command.
pub fn claim(session: Option<&str>, workspace: Option<&PathBuf>, dry_run: bool) -> Result<()> {
    let name_owned: String;
    let name: &str = match session {
        Some(s) => s,
        None => {
            // Peek at the workspace to find the first session name.
            let ws = crate::config::load(&crate::config::LoadOptions {
                workspace_path: workspace.cloned(),
                ..Default::default()
            })?;
            if let Some(first) = ws.sessions.first() {
                name_owned = first.name.clone();
                name_owned.as_str()
            } else {
                return Err(anyhow!("workspace {:?} has no sessions to claim", ws.name));
            }
        }
    };

    // Always takeover; that's the whole point of the verb.
    launch(name, workspace, dry_run, /* shared = */ false)
}

fn attach_mode_label(mode: AttachMode) -> &'static str {
    match mode {
        AttachMode::Takeover => "takeover: other clients will be detached",
        AttachMode::Shared => "shared: other clients stay attached",
    }
}

pub fn export(
    workspace: Option<&PathBuf>,
    format: Option<ExportFormatArg>,
    output: Option<&PathBuf>,
) -> Result<()> {
    let ws = load(&LoadOptions {
        workspace_path: workspace.cloned(),
        ..Default::default()
    })?;

    let format: crate::export::ExportFormat = format
        .map(Into::into)
        .unwrap_or_else(|| crate::export::ExportFormat::default_for(&ws));

    let rendered = crate::export::render(&ws, format);

    if let Some(path) = output {
        std::fs::write(path, &rendered)
            .with_context(|| format!("writing export to {}", path.display()))?;
    } else {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        stdout.write_all(rendered.as_bytes())?;
    }
    Ok(())
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
