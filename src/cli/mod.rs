//! CLI parsing and one-shot subcommands. The bare `pa` invocation drops into
//! the TUI; subcommands here are scriptable equivalents.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};

use crate::config::{load, LoadOptions};
use crate::domain::{Multiplexer as MpxEnum, Session, Workspace};
use crate::mux::{
    AttachMode, ClientCompletion, ClientExit, Multiplexer, TmuxAdapter, ZellijAdapter,
};
use crate::supervision::model::{parse_cpu_quota, parse_memory_size, parse_tasks_max};
use crate::supervision::{BindingReceipt, CapabilityState, MuxTarget, ResourceLimits};
#[cfg(target_os = "linux")]
use crate::supervision::{
    LimitKind, LogicalSessionId, MetricValue, OwnershipState, ResourceSnapshot, SupervisionBackend,
};

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

    /// Optional path to a directory or `*.portagenty.toml` file. When
    /// given without a subcommand, opens that workspace's TUI directly
    /// — no need to `cd` there first. Accepts either a workspace file
    /// or a directory (walks up from the directory).
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
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

        /// Resume prior state for kind-aware sessions. For
        /// `kind = "claude-code"` this appends `--continue` to the
        /// command string before launch. Silent no-op with a hint on
        /// other kinds — workspace TOML command strings stay literal
        /// so committed workspace files are reproducible.
        #[arg(long = "resume")]
        resume: bool,

        /// Kill any existing mpx session with this name before
        /// launching a fresh one. Useful on zellij where takeover
        /// isn't supported natively — "fresh launch" is the only
        /// way to guarantee other clients are disconnected. On tmux
        /// the default takeover already handles this cleanly; use
        /// `--fresh` only when you specifically want to wipe running
        /// state (reset to the workspace's declared command).
        #[arg(long = "fresh")]
        fresh: bool,

        /// Launch this session in an owned systemd user service with
        /// cgroup-v2 resource attribution. Currently implemented only on
        /// Linux hosts with a systemd user manager.
        #[arg(long = "supervise")]
        supervise: bool,

        /// Memory-reclaim threshold (IEC sizes such as 3G or 512MiB).
        /// Implies --supervise; this is MemoryHigh, not a hard OOM limit.
        #[arg(long = "memory-high")]
        memory_high: Option<String>,

        /// Hard memory limit (IEC sizes such as 5G or 4096MiB). Implies --supervise.
        #[arg(long = "memory-max")]
        memory_max: Option<String>,

        /// Hard swap limit (IEC sizes such as 512MiB). Implies --supervise.
        #[arg(long = "memory-swap-max")]
        memory_swap_max: Option<String>,

        /// CPU quota as a percentage. Values above 100 use multiple cores.
        /// Implies --supervise.
        #[arg(long = "cpu-quota")]
        cpu_quota: Option<String>,

        /// Maximum tasks/threads in the owned cgroup. Implies --supervise.
        #[arg(long = "tasks-max")]
        tasks_max: Option<String>,
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

        /// Resume prior state for kind-aware sessions. Same semantics
        /// as `pa launch --resume`.
        #[arg(long = "resume")]
        resume: bool,

        /// Kill any existing mpx session with this name before
        /// launching. Same semantics as `pa launch --fresh` — the
        /// zellij "takeover" workaround (loses running state).
        #[arg(long = "fresh")]
        fresh: bool,
    },
    /// Inspect and safely control Portagenty-owned resource containers.
    #[command(subcommand)]
    Resources(ResourcesCommand),
    /// Print the currently-resolved workspace (name, multiplexer,
    /// sessions) to stdout.
    List {
        /// Explicit path to a `*.portagenty.toml` file. When omitted,
        /// portagenty walks up from the current directory.
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<PathBuf>,
    },
    /// Scaffold a new `<name>.portagenty.toml` in the current
    /// directory. One starter session pre-populated so `pa` works
    /// immediately — edit or `pa add` more later. Designed for the
    /// phone-over-SSH case where you don't want to drop into nano.
    Init {
        /// Workspace name. Defaults to the current directory's name.
        name: Option<String>,

        /// Multiplexer to pin. Defaults to "tmux".
        #[arg(long = "mpx", value_enum)]
        mpx: Option<InitMpxArg>,

        /// Overwrite an existing workspace file if one's already here.
        #[arg(long = "force")]
        force: bool,

        /// Also scaffold `.mcp.json` + `.claude/commands/` +
        /// `.claude/skills/` so a Claude Code agent entering this
        /// workspace can discover portaconv (conversation extractor)
        /// and portagenty's workspace shape without the user having
        /// to explain either. Files are self-contained; skipped if
        /// already present. Prints an install hint for `pconv` if
        /// it's not on PATH — the hooks still work the moment pconv
        /// is installed. Safe to re-run against an already-scaffolded
        /// workspace: the existing TOML is left alone and only the
        /// missing hook files are written — pair it with `--force`
        /// only when you actually want to rewrite the TOML itself.
        #[arg(long = "with-agent-hooks")]
        with_agent_hooks: bool,
    },
    /// Append a new session to the current workspace file. Faster
    /// than editing TOML by hand — especially from Termux.
    Add {
        /// Session name.
        name: String,

        /// The command to run.
        #[arg(short = 'c', long = "command")]
        command: String,

        /// Session cwd. Defaults to "." (relative to the workspace
        /// file's directory).
        #[arg(long = "cwd")]
        cwd: Option<String>,

        /// Optional kind hint (claude-code / opencode / editor /
        /// dev-server / shell / other).
        #[arg(long = "kind", value_enum)]
        kind: Option<AddKindArg>,

        /// Optional human-readable note describing what the session
        /// is for. Display-only; shown dimmed next to the name in
        /// the TUI.
        #[arg(long = "description")]
        description: Option<String>,

        /// Explicit workspace file. Walks up from cwd otherwise.
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<PathBuf>,
    },
    /// Remove a session from the current workspace file. Preserves
    /// comments and formatting on everything else — only the matching
    /// `[[session]]` block is excised.
    Rm {
        /// Session name to remove.
        name: String,

        /// Explicit workspace file. Walks up from cwd otherwise.
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<PathBuf>,
    },
    /// Change session fields without opening an editor. Pass at
    /// most one of --command / --cwd / --kind / --rename / --description
    /// per call; --env KEY=VAL and --unset-env KEY are repeatable and
    /// stack freely with each other and with one field flag. Comments
    /// and formatting elsewhere in the file stay untouched.
    Edit {
        /// Name of the session to edit.
        name: String,

        /// New command (body of `command = "..."`).
        #[arg(long = "command")]
        command: Option<String>,

        /// New cwd.
        #[arg(long = "cwd")]
        cwd: Option<String>,

        /// New kind hint.
        #[arg(long = "kind", value_enum)]
        kind: Option<AddKindArg>,

        /// Rename the session. Errors if another session in the
        /// workspace already has this name.
        #[arg(long = "rename")]
        rename: Option<String>,

        /// Set the session's description note. Pass an empty string
        /// (`--description ""`) to clear it.
        #[arg(long = "description")]
        description: Option<String>,

        /// Set or update an env var on the session. Format is
        /// `KEY=VAL`. Repeatable. Combinable with other --env /
        /// --unset-env flags or with one of the field flags above.
        #[arg(long = "env")]
        env: Vec<String>,

        /// Remove an env var from the session by key. Repeatable.
        #[arg(long = "unset-env")]
        unset_env: Vec<String>,

        /// Explicit workspace file.
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<PathBuf>,
    },
    /// Manage bundled bash snippets — opt-in ergonomics
    /// (aliases, Termux-friendly tweaks) that ship with pa.
    #[command(subcommand)]
    Snippets(SnippetsCommand),
    /// Walk through the first-run wizard at any time. Scaffolds a
    /// workspace in the current directory, picks a multiplexer,
    /// optionally pre-populates a Claude Code session. Safe to re-run.
    Onboard,
    #[command(name = "__workload-anchor", hide = true)]
    WorkloadAnchor {
        #[arg(long)]
        spec: PathBuf,
    },
    /// Emit a shell completion script for the named shell. Pipe it
    /// into the completion file your shell loads — see the commands
    /// reference for per-shell install hints.
    Completions {
        /// Shell to emit completion for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
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
    /// Open a `pa://...` URL. This is the target of the OS-level URL
    /// scheme handler (see `pa protocol`). The URL dispatches to the
    /// matching pa action — `pa://open/<path>` opens a workspace TUI,
    /// `pa://shell/<path>` drops to a plain shell, etc.
    Open {
        /// The full `pa://...` URL, as delivered by the OS handler.
        url: String,
    },
    /// Manage the OS-level `pa://` URL scheme registration.
    #[command(subcommand)]
    Protocol(ProtocolCommand),
    /// Forward to `pconv` (portaconv) with this workspace's TOML
    /// as context. Thin shim: `pa convos list`, `pa convos dump <id>`,
    /// etc. dispatch to the matching `pconv` subcommand with
    /// `--workspace-toml <resolved-path>` automatically injected, so
    /// the agent CLI sees only this workspace's conversations. Any
    /// extra flags pass through unchanged. Errors cleanly with an
    /// install hint when `pconv` isn't on PATH — portagenty does
    /// NOT bundle portaconv; it's a separate crate.
    #[command(trailing_var_arg = true, allow_hyphen_values = true)]
    Convos {
        /// Explicit workspace TOML. Auto-walks up from `$PWD` when
        /// omitted. Forwarded to `pconv --workspace-toml`.
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<PathBuf>,

        /// The pconv subcommand and its args: e.g. `list`,
        /// `dump <session-id>`, `list --since 1d`. Passed through
        /// verbatim to `pconv`.
        #[arg(value_name = "PCONV_ARGS")]
        args: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ResourcesCommand {
    /// Report backend, metric, control, and resource-limit capabilities.
    Capabilities,
    /// Show ownership and current cgroup metrics for one or all declared sessions.
    Status {
        session: Option<String>,
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<PathBuf>,
    },
    /// Gracefully stop the exact receipted multiplexer target, then stop its
    /// verified systemd unit if descendants remain. Never sends SIGKILL.
    Stop {
        session: String,
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<PathBuf>,
    },
    /// Remove only proven-dead pending evidence or a proven-stale receipt.
    /// Never signals a process or stops a unit.
    Cleanup {
        session: String,
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<PathBuf>,
    },
    /// Force-kill the exact verified systemd control group. Requires --force.
    Kill {
        session: String,
        #[arg(long = "force", required = true)]
        force: bool,
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LaunchSupervisionOptions<'a> {
    pub enabled: bool,
    pub memory_high: Option<&'a str>,
    pub memory_max: Option<&'a str>,
    pub memory_swap_max: Option<&'a str>,
    pub cpu_quota: Option<&'a str>,
    pub tasks_max: Option<&'a str>,
}

#[derive(Debug, Subcommand)]
pub enum ProtocolCommand {
    /// List terminal emulators detected on this machine. The first
    /// entry is what `install` / `show` will pick if --terminal is
    /// not given.
    Terminals,
    /// Print the OS-appropriate registration snippet (a .desktop
    /// block, Windows .reg, or guidance) without writing anything.
    /// Copy-paste to apply manually if you'd rather.
    Show {
        /// Override the auto-detected terminal emulator. Matches
        /// case-insensitively against detected terminal names; also
        /// accepts a substring (e.g. "alac" → Alacritty).
        #[arg(long = "terminal")]
        terminal: Option<String>,
    },
    /// Install the `pa://` URL handler. Writes:
    ///   Linux → ~/.local/share/applications/portagenty.desktop
    ///   Windows → HKCU\Software\Classes\pa (user-scope, no admin)
    ///   macOS → errors with guidance (not automated yet)
    Install {
        /// Override the auto-detected terminal emulator.
        #[arg(long = "terminal")]
        terminal: Option<String>,
    },
    /// Remove a previously-installed registration.
    Uninstall,
    /// Report on what's currently registered for `pa://` on this
    /// machine. Useful for verifying install succeeded or debugging
    /// "why doesn't my pa:// link work?".
    Status,
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

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum InitMpxArg {
    Tmux,
    Zellij,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum AddKindArg {
    ClaudeCode,
    Opencode,
    Editor,
    DevServer,
    Shell,
    Other,
}

#[derive(Debug, Subcommand)]
pub enum SnippetsCommand {
    /// List every bundled snippet with a one-line description.
    List,
    /// Print a snippet's contents to stdout.
    Show {
        /// Snippet name (see `pa snippets list`).
        name: String,
    },
    /// Append or update a snippet in your rc file. Idempotent —
    /// repeated installs replace the block in-place instead of
    /// duplicating. Other content in the rc file is preserved
    /// verbatim.
    Install {
        /// Snippet name.
        name: String,
        /// Target file. Defaults to `$HOME/.bashrc`. Pass your
        /// actual rc (`~/.zshrc`, `~/.config/fish/config.fish`,
        /// etc.) if bash isn't your shell — the snippets are
        /// POSIX-ish and will run under zsh; fish users should
        /// translate by hand until we ship fish snippets.
        #[arg(long = "to")]
        to: Option<PathBuf>,
        /// Print what would be written without modifying the file.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Remove a previously-installed snippet from your rc file.
    Uninstall {
        name: String,
        #[arg(long = "from")]
        from: Option<PathBuf>,
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

impl From<AddKindArg> for crate::domain::SessionKind {
    fn from(a: AddKindArg) -> Self {
        use crate::domain::SessionKind;
        match a {
            AddKindArg::ClaudeCode => SessionKind::ClaudeCode,
            AddKindArg::Opencode => SessionKind::Opencode,
            AddKindArg::Editor => SessionKind::Editor,
            AddKindArg::DevServer => SessionKind::DevServer,
            AddKindArg::Shell => SessionKind::Shell,
            AddKindArg::Other => SessionKind::Other,
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
    resume: bool,
    fresh: bool,
    supervision: LaunchSupervisionOptions<'_>,
) -> Result<()> {
    let (mut sess, ws) = resolve(session, workspace)?;
    let mode = if shared {
        AttachMode::Shared
    } else {
        AttachMode::Takeover
    };

    if resume {
        apply_resume_modifier(&mut sess)?;
    }

    let limits = parse_resource_limits(
        supervision.memory_high,
        supervision.memory_max,
        supervision.memory_swap_max,
        supervision.cpu_quota,
        supervision.tasks_max,
    )?;
    if supervision.enabled || !limits.is_empty() {
        let limits = limits.resolve_for_kind(sess.kind)?;
        let workspace_name = ws.name.clone();
        let session_name = sess.name.clone();
        let result = launch_supervised_resolved(sess, ws, dry_run, mode, fresh, limits)?;
        if dry_run {
            return Ok(());
        }
        return finish_client_return(result, &workspace_name, &session_name);
    }

    let workspace_name = ws.name.clone();
    let session_name = sess.name.clone();
    let mux = build_mux(ws.multiplexer)?;
    let mpx_name = crate::mux::workspace_session_name(&ws.name, &sess.name);

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
        if fresh {
            writeln!(
                out,
                "  fresh:   true (would kill any existing mpx session {mpx_name:?} first)"
            )?;
        }
        return Ok(());
    }

    // --fresh: kill any existing session with this name before
    // launching. For zellij this is the only way to guarantee
    // takeover semantics — other clients get dropped because the
    // session they were attached to is gone. For tmux it's
    // overkill (the default takeover already kicks clients without
    // destroying state) but respected if explicitly asked.
    if fresh {
        if let Ok(true) = mux.has_session(&mpx_name) {
            mux.kill(&mpx_name).with_context(|| {
                format!("killing existing session {mpx_name:?} before fresh launch")
            })?;
        }
    }

    // Record the launch BEFORE attaching — attach blocks until the
    // user detaches from the mpx, so recording after could lose the
    // entry if the process is killed mid-session.
    if let Some(path) = &ws.file_path {
        let _ = crate::state::record_launch(path, &sess.name);
    }

    let completion = mux
        .create_and_attach(&sess, &mpx_name, mode)
        .with_context(|| format!("launching session {:?}", sess.name))?
        .map(|_| ());
    finish_client_return(completion, &workspace_name, &session_name)
}

pub(crate) fn return_banner(workspace_name: &str, session_name: &str) -> String {
    format!(
        "pa ← returned from {:?}",
        format!("{workspace_name} / {session_name}")
    )
}

pub(crate) fn print_return_banner(workspace_name: &str, session_name: &str) {
    eprintln!();
    eprintln!("  {}", return_banner(workspace_name, session_name));
    eprintln!();
}

pub(crate) fn client_exit_message(exit: ClientExit) -> String {
    match (exit.code, exit.signal) {
        (Some(code), _) => format!("multiplexer client exited abnormally with code {code}"),
        (None, Some(signal)) => format!("multiplexer client was terminated by signal {signal}"),
        (None, None) => "multiplexer client exited abnormally".into(),
    }
}

pub(crate) fn finish_client_return(
    completion: ClientCompletion<()>,
    workspace_name: &str,
    session_name: &str,
) -> Result<()> {
    print_return_banner(workspace_name, session_name);
    match completion {
        ClientCompletion::Returned(()) => Ok(()),
        ClientCompletion::Abnormal(exit) => {
            let message = client_exit_message(exit);
            eprintln!("  pa: {message}");
            Err(anyhow!(message))
        }
    }
}

fn parse_resource_limits(
    memory_high: Option<&str>,
    memory_max: Option<&str>,
    memory_swap_max: Option<&str>,
    cpu_quota: Option<&str>,
    tasks_max: Option<&str>,
) -> Result<ResourceLimits> {
    let limits = ResourceLimits {
        memory_high_bytes: memory_high.map(parse_memory_size).transpose()?,
        memory_max_bytes: memory_max.map(parse_memory_size).transpose()?,
        memory_swap_max_bytes: memory_swap_max.map(parse_memory_size).transpose()?,
        cpu_quota_percent: cpu_quota.map(parse_cpu_quota).transpose()?,
        tasks_max: tasks_max.map(parse_tasks_max).transpose()?,
    };
    limits.validate_consistency()?;
    Ok(limits)
}

pub(crate) enum RoutineSupervisedLaunch {
    #[cfg(target_os = "linux")]
    ClientReturned(ClientCompletion<()>),
    FallbackSafe(anyhow::Error),
}

#[cfg(target_os = "linux")]
pub(crate) fn launch_supervised_resolved(
    sess: Session,
    ws: Workspace,
    dry_run: bool,
    mode: AttachMode,
    fresh: bool,
    limits: ResourceLimits,
) -> Result<ClientCompletion<()>> {
    match launch_supervised_inner(sess, ws, dry_run, mode, fresh, limits, false)? {
        RoutineSupervisedLaunch::ClientReturned(completion) => Ok(completion),
        RoutineSupervisedLaunch::FallbackSafe(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn launch_supervised_routine_resolved(
    sess: Session,
    ws: Workspace,
    mode: AttachMode,
    limits: ResourceLimits,
) -> Result<RoutineSupervisedLaunch> {
    launch_supervised_inner(sess, ws, false, mode, false, limits, true)
}

#[cfg(target_os = "linux")]
fn launch_supervised_inner(
    sess: Session,
    ws: Workspace,
    dry_run: bool,
    mode: AttachMode,
    fresh: bool,
    limits: ResourceLimits,
    allow_fallback: bool,
) -> Result<RoutineSupervisedLaunch> {
    let workspace_id = ws.id.as_deref().ok_or_else(|| {
        anyhow!(
            "supervised launch requires a workspace UUID; open the workspace in the TUI, press `S` on an idle session, and confirm adding a stable ID (reopen before retrying only if you cancel after assignment)"
        )
    })?;
    let logical_id = LogicalSessionId::new(workspace_id, sess.name.clone())?;

    if dry_run {
        let out = io::stdout();
        let mut out = out.lock();
        writeln!(
            out,
            "would launch {:?} under Linux systemd user supervision",
            sess.name
        )?;
        writeln!(out, "  multiplexer: {:?}", ws.multiplexer)?;
        writeln!(out, "  cwd:         {}", sess.cwd.display())?;
        writeln!(out, "  command:     {}", sess.command)?;
        print_limits(&mut out, &limits)?;
        if fresh {
            writeln!(
                out,
                "  fresh:       refused if an owned supervision receipt already exists"
            )?;
        }
        return Ok(RoutineSupervisedLaunch::ClientReturned(
            ClientCompletion::Returned(()),
        ));
    }

    let store = crate::supervision::ReceiptStore::standard()?;
    if let Some(pending) = store.find_pending(&logical_id)? {
        bail!(
            "a pending supervision launch blocks ordinary fallback and new creation: unit={:?}, target={:?}, last_error={:?}; inspect with `pa resources status {:?}` and clear only proven-dead evidence with `pa resources cleanup {:?}`",
            pending.unit_name,
            pending.mux_target,
            pending.last_error,
            sess.name,
            sess.name
        );
    }
    let existing = store.find(&logical_id)?;
    let ordinary_target = crate::mux::workspace_session_name(&ws.name, &sess.name);
    if existing.is_none() {
        let ordinary_mux = build_mux(ws.multiplexer)?;
        if ordinary_mux.has_session(&ordinary_target)? {
            bail!(
                "an existing unverified multiplexer session {ordinary_target:?} is already live; Portagenty will not claim it as supervised"
            );
        }
    }

    let backend = match crate::supervision::LinuxSystemdBackend::connect() {
        Ok(backend) => backend,
        Err(error) if allow_fallback && existing.is_none() => {
            return Ok(RoutineSupervisedLaunch::FallbackSafe(error.context(
                "connecting to Linux systemd user supervision before any workload was created",
            )));
        }
        Err(error) => return Err(error),
    };
    let capabilities = backend.capabilities();
    let unavailable = if capabilities.overall != CapabilityState::Supported {
        Some(anyhow!(
            "resource supervision is unavailable: {:?}",
            capabilities.overall
        ))
    } else {
        [
            (LimitKind::MemoryHigh, limits.memory_high_bytes.is_some()),
            (LimitKind::MemoryMax, limits.memory_max_bytes.is_some()),
            (
                LimitKind::MemorySwapMax,
                limits.memory_swap_max_bytes.is_some(),
            ),
            (LimitKind::CpuQuota, limits.cpu_quota_percent.is_some()),
            (LimitKind::TasksMax, limits.tasks_max.is_some()),
        ]
        .into_iter()
        .find_map(|(kind, requested)| {
            if !requested {
                return None;
            }
            let state = capabilities.limits.get(&kind);
            (state != Some(&CapabilityState::Supported))
                .then(|| anyhow!("requested {kind:?} resource limit is unavailable: {state:?}"))
        })
    };
    if let Some(error) = unavailable {
        if allow_fallback && existing.is_none() {
            return Ok(RoutineSupervisedLaunch::FallbackSafe(error));
        }
        return Err(error);
    }
    if fresh && existing.is_some() {
        bail!(
            "--fresh cannot replace an owned supervised workload; use `pa resources stop {:?}` first",
            sess.name
        );
    }
    if let Some(receipt) = &existing {
        match backend.reconcile(receipt)? {
            OwnershipState::LegacyRestartRequired(reason) => {
                eprintln!("pa: legacy supervised service is attach-only: {reason}");
                return Ok(RoutineSupervisedLaunch::ClientReturned(
                    attach_receipted_target(&receipt.mux_target, mode)?,
                ));
            }
            OwnershipState::SplitContainment(reason) => {
                eprintln!("pa: split containment; attaching without resource ownership: {reason}");
                return Ok(RoutineSupervisedLaunch::ClientReturned(
                    attach_receipted_target(&receipt.mux_target, mode)?,
                ));
            }
            OwnershipState::OwnedVerified(_) => {}
            state => bail!("existing supervision receipt is not attachable: {state:?}"),
        }
        if !allow_fallback && !limits.is_empty() && receipt.limits != limits {
            bail!(
                "this supervised session already exists with different resource limits; stop it before launching with new limits"
            );
        }
    } else {
        let ordinary_mux = build_mux(ws.multiplexer)?;
        if ordinary_mux.has_session(&ordinary_target)? {
            bail!(
                "an existing unverified multiplexer session {ordinary_target:?} appeared during supervised preflight; Portagenty will not claim it"
            );
        }
    }

    if let Some(path) = &ws.file_path {
        let _ = crate::state::record_launch(path, &sess.name);
    }
    let receipt = match ws.multiplexer {
        MpxEnum::Tmux => backend.create_tmux_binding(&store, logical_id, &sess, limits)?,
        MpxEnum::Zellij => {
            backend.create_zellij_binding(&store, logical_id, &ws.name, &sess, limits)?
        }
        MpxEnum::Wezterm => bail!("supervised WezTerm sessions are not supported"),
    };
    let before = backend.snapshot(&receipt, None).ok();
    let completion = attach_receipted_target(&receipt.mux_target, mode)
        .with_context(|| format!("attaching to supervised session {:?}", sess.name))?;
    if let Ok(after) = backend.snapshot(&receipt, before.as_ref()) {
        print_resource_event_notice(before.as_ref(), &after);
    }
    Ok(RoutineSupervisedLaunch::ClientReturned(completion))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn launch_supervised_resolved(
    _sess: Session,
    _ws: Workspace,
    _dry_run: bool,
    _mode: AttachMode,
    _fresh: bool,
    _limits: ResourceLimits,
) -> Result<ClientCompletion<()>> {
    bail!(
        "resource supervision is currently implemented only on Linux with systemd user services and cgroup v2"
    )
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn launch_supervised_routine_resolved(
    _sess: Session,
    _ws: Workspace,
    _mode: AttachMode,
    _limits: ResourceLimits,
) -> Result<RoutineSupervisedLaunch> {
    Ok(RoutineSupervisedLaunch::FallbackSafe(anyhow!(
        "resource supervision is currently implemented only on Linux with systemd user services and cgroup v2"
    )))
}

#[cfg(target_os = "linux")]
fn effective_stale_replacement_limits(
    kind: Option<crate::domain::SessionKind>,
    legacy_receipt: bool,
    requested: ResourceLimits,
) -> Result<ResourceLimits> {
    if legacy_receipt {
        Ok(ResourceLimits::defaults_for_kind(kind))
    } else {
        requested.resolve_for_kind(kind)
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn replace_stale_supervised_resolved(
    sess: Session,
    ws: Workspace,
    expected: BindingReceipt,
    mode: AttachMode,
    limits: ResourceLimits,
) -> Result<ClientCompletion<()>> {
    let limits = effective_stale_replacement_limits(sess.kind, expected.is_legacy(), limits)?;
    let workspace_id = ws
        .id
        .as_deref()
        .ok_or_else(|| anyhow!("stale supervised replacement requires a valid workspace UUID"))?;
    let logical_id = LogicalSessionId::new(workspace_id, sess.name.clone())?;
    if expected.logical_id != logical_id {
        bail!("the confirmed stale receipt does not match the selected workspace/session");
    }
    let store = crate::supervision::ReceiptStore::standard()?;
    if store.find_pending(&logical_id)?.is_some() {
        bail!("a pending supervision launch exists; no stale receipt was removed");
    }
    if store.find(&logical_id)?.as_ref() != Some(&expected) {
        bail!("the ownership receipt changed after confirmation; refresh and retry");
    }
    let ordinary_target = crate::mux::workspace_session_name(&ws.name, &sess.name);
    let ordinary_mux = build_mux(ws.multiplexer)?;
    if ordinary_mux.has_session(&ordinary_target)? {
        bail!(
            "an ordinary multiplexer target {ordinary_target:?} is live; no stale receipt was removed"
        );
    }
    let backend = crate::supervision::LinuxSystemdBackend::connect()?;
    let capabilities = backend.capabilities();
    if capabilities.overall != CapabilityState::Supported {
        bail!(
            "resource supervision is unavailable: {:?}; no stale receipt was removed",
            capabilities.overall
        );
    }
    for (kind, requested) in [
        (LimitKind::MemoryHigh, limits.memory_high_bytes.is_some()),
        (LimitKind::MemoryMax, limits.memory_max_bytes.is_some()),
        (
            LimitKind::MemorySwapMax,
            limits.memory_swap_max_bytes.is_some(),
        ),
        (LimitKind::CpuQuota, limits.cpu_quota_percent.is_some()),
        (LimitKind::TasksMax, limits.tasks_max.is_some()),
    ] {
        if requested && capabilities.limits.get(&kind) != Some(&CapabilityState::Supported) {
            bail!(
                "requested {kind:?} resource limit is unavailable: {:?}; no stale receipt was removed",
                capabilities.limits.get(&kind)
            );
        }
    }

    backend.remove_stale_binding(&store, &expected)?;
    if ordinary_mux.has_session(&ordinary_target)? {
        bail!(
            "an ordinary multiplexer target {ordinary_target:?} appeared after stale cleanup; refusing supervised creation"
        );
    }
    if let Some(path) = &ws.file_path {
        let _ = crate::state::record_launch(path, &sess.name);
    }
    let receipt = match ws.multiplexer {
        MpxEnum::Tmux => backend.create_tmux_binding(&store, logical_id, &sess, limits)?,
        MpxEnum::Zellij => {
            backend.create_zellij_binding(&store, logical_id, &ws.name, &sess, limits)?
        }
        MpxEnum::Wezterm => bail!("supervised WezTerm sessions are not supported"),
    };
    let before = backend.snapshot(&receipt, None).ok();
    let completion = attach_receipted_target(&receipt.mux_target, mode)
        .with_context(|| format!("attaching to supervised session {:?}", sess.name))?;
    if let Ok(after) = backend.snapshot(&receipt, before.as_ref()) {
        print_resource_event_notice(before.as_ref(), &after);
    }
    Ok(completion)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn replace_stale_supervised_resolved(
    _sess: Session,
    _ws: Workspace,
    _expected: BindingReceipt,
    _mode: AttachMode,
    _limits: ResourceLimits,
) -> Result<ClientCompletion<()>> {
    bail!("stale supervised replacement is currently supported only on Linux")
}

#[cfg(target_os = "linux")]
pub(crate) fn attach_receipted_target(
    target: &MuxTarget,
    mode: AttachMode,
) -> Result<ClientCompletion<()>> {
    match target {
        MuxTarget::TmuxPrivate { socket, session } => {
            TmuxAdapter::with_socket(socket).attach(session, mode)
        }
        MuxTarget::Zellij {
            session,
            runtime_dir: Some(runtime_dir),
        } => ZellijAdapter::with_runtime_dir(runtime_dir).attach(session, mode),
        _ => bail!("receipt does not contain an exact supervised multiplexer target"),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn attach_receipted_target(
    _target: &MuxTarget,
    _mode: AttachMode,
) -> Result<ClientCompletion<()>> {
    bail!("receipted resource targets are currently supported only on Linux")
}

#[cfg(target_os = "linux")]
fn print_limits(out: &mut impl Write, limits: &ResourceLimits) -> Result<()> {
    writeln!(
        out,
        "  memory high: {}",
        limits
            .memory_high_bytes
            .map(format_bytes)
            .unwrap_or_else(|| "not set".into())
    )?;
    writeln!(
        out,
        "  memory max:  {}",
        limits
            .memory_max_bytes
            .map(format_bytes)
            .unwrap_or_else(|| "not set".into())
    )?;
    writeln!(
        out,
        "  swap max:    {}",
        limits
            .memory_swap_max_bytes
            .map(format_bytes)
            .unwrap_or_else(|| "not set".into())
    )?;
    writeln!(
        out,
        "  CPU quota:  {}",
        limits
            .cpu_quota_percent
            .map(|value| format!("{value}%"))
            .unwrap_or_else(|| "not set".into())
    )?;
    writeln!(
        out,
        "  tasks max:  {}",
        limits
            .tasks_max
            .map(|value| value.to_string())
            .unwrap_or_else(|| "not set".into())
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn format_bytes(value: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let value_f64 = value as f64;
    if value_f64 >= GIB {
        format!("{:.2} GiB", value_f64 / GIB)
    } else if value_f64 >= MIB {
        format!("{:.2} MiB", value_f64 / MIB)
    } else if value_f64 >= KIB {
        format!("{:.2} KiB", value_f64 / KIB)
    } else {
        format!("{value} B")
    }
}

#[cfg(target_os = "linux")]
fn metric_counter(snapshot: &ResourceSnapshot, group: &str, key: &str) -> Option<u64> {
    let metric = match group {
        "memory" => &snapshot.memory_events,
        "swap" => &snapshot.swap_events,
        "tasks" => &snapshot.tasks_events,
        _ => return None,
    };
    match metric {
        MetricValue::Value(values) => values.get(key).copied(),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn print_resource_event_notice(previous: Option<&ResourceSnapshot>, current: &ResourceSnapshot) {
    let Some(previous) = previous else {
        return;
    };
    let mut notices = Vec::new();
    for (group, key, label) in [
        ("memory", "high", "MemoryHigh reclaim events"),
        ("memory", "oom", "cgroup OOM events"),
        ("memory", "oom_kill", "kernel OOM kills"),
        ("tasks", "max", "TasksMax rejections"),
    ] {
        let old = metric_counter(previous, group, key).unwrap_or(0);
        let new = metric_counter(current, group, key).unwrap_or(0);
        if new > old {
            notices.push(format!("{label}: +{}", new - old));
        }
    }
    let old_throttled = match &previous.cpu {
        MetricValue::Value(cpu) => cpu.extra.get("nr_throttled").copied().unwrap_or(0),
        _ => 0,
    };
    let new_throttled = match &current.cpu {
        MetricValue::Value(cpu) => cpu.extra.get("nr_throttled").copied().unwrap_or(0),
        _ => 0,
    };
    if new_throttled > old_throttled {
        notices.push(format!(
            "CPU quota throttle periods: +{}",
            new_throttled - old_throttled
        ));
    }
    if !notices.is_empty() {
        eprintln!();
        eprintln!("  pa: resource events occurred while this session was attached:");
        for notice in notices {
            eprintln!("  - {notice}");
        }
        eprintln!("  Run `pa resources status` for the current counters and limits.");
    }
}

pub fn resources(command: ResourcesCommand) -> Result<()> {
    match command {
        ResourcesCommand::Capabilities => resources_capabilities(),
        ResourcesCommand::Status { session, workspace } => {
            resources_status(session.as_deref(), workspace.as_ref())
        }
        ResourcesCommand::Stop { session, workspace } => {
            resources_stop(&session, workspace.as_ref())
        }
        ResourcesCommand::Cleanup { session, workspace } => {
            resources_cleanup(&session, workspace.as_ref())
        }
        ResourcesCommand::Kill {
            session,
            force,
            workspace,
        } => resources_kill(&session, force, workspace.as_ref()),
    }
}

fn resources_capabilities() -> Result<()> {
    let report = crate::supervision::platform_backend().capabilities();
    println!("backend: {:?}", report.backend);
    println!("overall: {}", capability_label(&report.overall));
    println!("metrics:");
    for (kind, state) in report.metrics {
        println!("  {kind:?}: {}", capability_label(&state));
    }
    println!("actions:");
    for (kind, state) in report.actions {
        println!("  {kind:?}: {}", capability_label(&state));
    }
    println!("resource limits:");
    for (kind, state) in report.limits {
        println!("  {kind:?}: {}", capability_label(&state));
    }
    for note in report.notes {
        println!("note: {note}");
    }
    Ok(())
}

fn capability_label(state: &CapabilityState) -> String {
    match state {
        CapabilityState::Supported => "supported".into(),
        CapabilityState::Unavailable(reason) => format!("unavailable: {reason}"),
        CapabilityState::NotImplemented => "not implemented".into(),
        CapabilityState::Unsupported => "unsupported".into(),
    }
}

#[cfg(target_os = "linux")]
fn resources_status(session: Option<&str>, workspace: Option<&PathBuf>) -> Result<()> {
    let ws = load(&LoadOptions {
        workspace_path: workspace.cloned(),
        ..Default::default()
    })?;
    let backend = crate::supervision::LinuxSystemdBackend::connect()?;
    let store = crate::supervision::ReceiptStore::standard()?;
    let sessions: Vec<&Session> = match session {
        Some(name) => vec![ws
            .sessions
            .iter()
            .find(|candidate| candidate.name == name)
            .ok_or_else(|| anyhow!("no session named {name:?} in workspace {:?}", ws.name))?],
        None => ws.sessions.iter().collect(),
    };
    if sessions.is_empty() {
        println!("workspace {:?} has no declared sessions", ws.name);
        return Ok(());
    }
    for (index, declared) in sessions.iter().enumerate() {
        if index > 0 {
            println!();
        }
        print_resource_status(&ws, declared, &backend, &store)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn resources_status(_session: Option<&str>, _workspace: Option<&PathBuf>) -> Result<()> {
    bail!(
        "resource status is currently implemented only on Linux with systemd user services and cgroup v2"
    )
}

#[cfg(target_os = "linux")]
fn print_resource_status(
    ws: &Workspace,
    session: &Session,
    backend: &crate::supervision::LinuxSystemdBackend,
    store: &crate::supervision::ReceiptStore,
) -> Result<()> {
    println!("session: {}", session.name);
    let Some(workspace_id) = ws.id.as_deref() else {
        println!("ownership: unsupported (workspace has no UUID)");
        return Ok(());
    };
    let logical_id = LogicalSessionId::new(workspace_id, session.name.clone())?;
    if let Some(pending) = store.find_pending(&logical_id)? {
        println!("ownership: pending-launch");
        println!("unit: {}", pending.unit_name);
        println!("target: {:?}", pending.mux_target);
        println!("marker: {}", pending.marker_path.display());
        println!(
            "creator: pid={} start-time-ticks={}",
            pending.creator_pid, pending.creator_start_time_ticks
        );
        if let Some(error) = &pending.last_error {
            println!("last error: {error}");
        }
        match backend.reconcile_pending(&pending)? {
            crate::supervision::PendingLaunchState::Active(reason) => {
                println!("pending state: active");
                println!("evidence: {reason}");
            }
            crate::supervision::PendingLaunchState::Dead(reason) => {
                println!("pending state: dead-cleanable");
                println!("evidence: {reason}");
                println!(
                    "note: run `pa resources cleanup {:?}` to remove this signal-free journal entry",
                    session.name
                );
            }
            crate::supervision::PendingLaunchState::Ambiguous(reason) => {
                println!("pending state: ambiguous");
                println!("evidence: {reason}");
                println!("note: attach, fallback, creation, stop, and kill remain blocked");
            }
        }
        return Ok(());
    }
    let Some(receipt) = store.find(&logical_id)? else {
        let ordinary_target = crate::mux::workspace_session_name(&ws.name, &session.name);
        let live = build_mux(ws.multiplexer)
            .and_then(|mux| mux.has_session(&ordinary_target))
            .unwrap_or(false);
        println!(
            "ownership: {}",
            if live {
                "existing-unverified"
            } else {
                "idle-supported"
            }
        );
        if live {
            println!(
                "note: live multiplexer target {ordinary_target:?} is not cgroup-owned by Portagenty"
            );
        }
        return Ok(());
    };

    match backend.reconcile(&receipt)? {
        OwnershipState::OwnedVerified(_) => {
            println!("ownership: owned-and-verified");
            println!("unit: {}", receipt.unit_name);
            println!("invocation: {}", receipt.invocation_id);
            println!("control group: {}", receipt.control_group);
            println!("target: {:?}", receipt.mux_target);
            let mut out = io::stdout().lock();
            print_limits(&mut out, &receipt.limits)?;
            drop(out);
            let snapshot = backend.snapshot(&receipt, None)?;
            print_snapshot(&snapshot);
        }
        OwnershipState::LegacyRestartRequired(reason) => {
            println!("ownership: legacy-restart-required");
            println!("reason: {reason}");
            println!("target: {:?}", receipt.mux_target);
            println!("note: attach is allowed; metrics and resource control are withheld");
        }
        OwnershipState::SplitContainment(reason) => {
            println!("ownership: split-containment");
            println!("reason: {reason}");
            println!("target: {:?}", receipt.mux_target);
            println!("note: attach is allowed; external descendants are not covered by service stop or force-kill");
        }
        OwnershipState::AmbiguousBinding(reason) => {
            println!("ownership: ambiguous-binding");
            println!("reason: {reason}");
        }
        OwnershipState::StaleBinding(reason) => {
            println!("ownership: stale-binding");
            println!("reason: {reason}");
        }
        state => println!("ownership: {state:?}"),
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn print_snapshot(snapshot: &ResourceSnapshot) {
    println!(
        "captured at: {} ms since epoch",
        snapshot.captured_at_unix_ms
    );
    println!("CPU: {}", metric_debug(&snapshot.cpu));
    println!("CPU rate: {}", metric_number(&snapshot.cpu_percent, "%"));
    println!(
        "memory current: {}",
        metric_bytes(&snapshot.memory_current_bytes)
    );
    println!("memory peak: {}", metric_bytes(&snapshot.memory_peak_bytes));
    println!("memory events: {}", metric_debug(&snapshot.memory_events));
    println!(
        "swap current: {}",
        metric_bytes(&snapshot.swap_current_bytes)
    );
    println!("swap peak: {}", metric_bytes(&snapshot.swap_peak_bytes));
    println!("swap events: {}", metric_debug(&snapshot.swap_events));
    println!(
        "tasks/threads current: {}",
        metric_u64(&snapshot.tasks_current)
    );
    println!("tasks/threads peak: {}", metric_u64(&snapshot.tasks_peak));
    println!("tasks events: {}", metric_debug(&snapshot.tasks_events));
    println!("I/O totals: {}", metric_debug(&snapshot.io_totals));
    println!(
        "I/O read rate: {}",
        metric_number(&snapshot.io_read_bytes_per_sec, " B/s")
    );
    println!(
        "I/O write rate: {}",
        metric_number(&snapshot.io_write_bytes_per_sec, " B/s")
    );
    println!("CPU pressure: {}", metric_debug(&snapshot.cpu_pressure));
    println!(
        "memory pressure: {}",
        metric_debug(&snapshot.memory_pressure)
    );
    println!("I/O pressure: {}", metric_debug(&snapshot.io_pressure));
    println!("cgroup state: {}", metric_debug(&snapshot.cgroup_state));
    println!(
        "note: memory is cgroup-charged usage; tasks count threads; one-shot rates require a prior sample"
    );
}

#[cfg(target_os = "linux")]
fn metric_bytes(metric: &MetricValue<u64>) -> String {
    match metric {
        MetricValue::Value(value) => format_bytes(*value),
        MetricValue::Unsupported => "unsupported".into(),
        MetricValue::Unavailable(reason) => format!("unavailable: {reason}"),
        MetricValue::Error(reason) => format!("error: {reason}"),
    }
}

#[cfg(target_os = "linux")]
fn metric_u64(metric: &MetricValue<u64>) -> String {
    match metric {
        MetricValue::Value(value) => value.to_string(),
        MetricValue::Unsupported => "unsupported".into(),
        MetricValue::Unavailable(reason) => format!("unavailable: {reason}"),
        MetricValue::Error(reason) => format!("error: {reason}"),
    }
}

#[cfg(target_os = "linux")]
fn metric_number(metric: &MetricValue<f64>, suffix: &str) -> String {
    match metric {
        MetricValue::Value(value) => format!("{value:.2}{suffix}"),
        MetricValue::Unsupported => "unsupported".into(),
        MetricValue::Unavailable(reason) => format!("unavailable: {reason}"),
        MetricValue::Error(reason) => format!("error: {reason}"),
    }
}

#[cfg(target_os = "linux")]
fn metric_debug<T: std::fmt::Debug>(metric: &MetricValue<T>) -> String {
    match metric {
        MetricValue::Value(value) => format!("{value:?}"),
        MetricValue::Unsupported => "unsupported".into(),
        MetricValue::Unavailable(reason) => format!("unavailable: {reason}"),
        MetricValue::Error(reason) => format!("error: {reason}"),
    }
}

#[cfg(target_os = "linux")]
fn owned_resource_binding(
    session: &str,
    workspace: Option<&PathBuf>,
) -> Result<(
    BindingReceipt,
    crate::supervision::LinuxSystemdBackend,
    crate::supervision::ReceiptStore,
)> {
    let (declared, ws) = resolve(session, workspace)?;
    let workspace_id = ws
        .id
        .as_deref()
        .ok_or_else(|| anyhow!("resource control requires a workspace UUID"))?;
    let logical_id = LogicalSessionId::new(workspace_id, declared.name)?;
    let store = crate::supervision::ReceiptStore::standard()?;
    if let Some(pending) = store.find_pending(&logical_id)? {
        bail!(
            "refusing resource control while a pending launch exists: unit={:?}, target={:?}; use `pa resources status {:?}`",
            pending.unit_name,
            pending.mux_target,
            session
        );
    }
    let receipt = store
        .find(&logical_id)?
        .ok_or_else(|| anyhow!("session {session:?} has no Portagenty supervision receipt"))?;
    let backend = crate::supervision::LinuxSystemdBackend::connect()?;
    match backend.reconcile(&receipt)? {
        OwnershipState::OwnedVerified(_) => Ok((receipt, backend, store)),
        OwnershipState::StaleBinding(reason) => {
            bail!("refusing resource control for a stale binding: {reason}")
        }
        state => bail!("refusing resource control for ownership state {state:?}"),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn graceful_stop_target(target: &MuxTarget) -> Result<()> {
    match target {
        MuxTarget::TmuxPrivate { socket, session } => {
            TmuxAdapter::with_socket(socket).kill(session)
        }
        MuxTarget::Zellij {
            session,
            runtime_dir: Some(runtime_dir),
        } => ZellijAdapter::with_runtime_dir(runtime_dir).kill(session),
        _ => bail!("receipt does not contain an exact controllable multiplexer target"),
    }
}

#[cfg(target_os = "linux")]
fn resources_cleanup(session: &str, workspace: Option<&PathBuf>) -> Result<()> {
    let (declared, ws) = resolve(session, workspace)?;
    let workspace_id = ws
        .id
        .as_deref()
        .ok_or_else(|| anyhow!("resource cleanup requires a workspace UUID"))?;
    let logical_id = LogicalSessionId::new(workspace_id, declared.name)?;
    let store = crate::supervision::ReceiptStore::standard()?;
    let backend = crate::supervision::LinuxSystemdBackend::connect()?;
    if let Some(pending) = store.find_pending(&logical_id)? {
        backend.remove_dead_pending(&store, &pending)?;
        println!("removed proven-dead pending launch evidence without signalling any process");
        return Ok(());
    }
    let receipt = store
        .find(&logical_id)?
        .ok_or_else(|| anyhow!("session {session:?} has no supervision evidence to clean"))?;
    match backend.reconcile(&receipt)? {
        OwnershipState::StaleBinding(_) => {
            backend.remove_stale_binding(&store, &receipt)?;
            println!("removed proven-stale supervision receipt without signalling any process");
            Ok(())
        }
        state => bail!("refusing signal-free cleanup for ownership state {state:?}"),
    }
}

#[cfg(not(target_os = "linux"))]
fn resources_cleanup(_session: &str, _workspace: Option<&PathBuf>) -> Result<()> {
    bail!("resource cleanup is currently implemented only on Linux")
}

#[cfg(target_os = "linux")]
fn resources_stop(session: &str, workspace: Option<&PathBuf>) -> Result<()> {
    let (receipt, backend, store) = owned_resource_binding(session, workspace)?;
    if let Err(error) = graceful_stop_target(&receipt.mux_target) {
        eprintln!("warning: graceful multiplexer shutdown failed: {error:#}");
        eprintln!("continuing with a non-force systemd stop for the verified control group");
    }
    let result = backend.stop_unit(&receipt)?;
    println!("{}", result.final_state);
    if result.completed {
        let _ = store.remove(&receipt.logical_id)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn resources_stop(_session: &str, _workspace: Option<&PathBuf>) -> Result<()> {
    bail!("resource control is currently implemented only on Linux")
}

#[cfg(target_os = "linux")]
fn resources_kill(session: &str, force: bool, workspace: Option<&PathBuf>) -> Result<()> {
    if !force {
        bail!("force-kill requires the explicit --force flag");
    }
    let (receipt, backend, store) = owned_resource_binding(session, workspace)?;
    let result = backend.force_kill(&receipt)?;
    println!("{}", result.final_state);
    if result.completed {
        let _ = store.remove(&receipt.logical_id)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn resources_kill(_session: &str, _force: bool, _workspace: Option<&PathBuf>) -> Result<()> {
    bail!("resource control is currently implemented only on Linux")
}

/// Mutate the session's command in-place to resume prior state,
/// based on its `kind:` hint. For unknown kinds we leave the command
/// alone and print a one-liner to stderr so the user knows `--resume`
/// was a no-op on this row (vs. silently ignored).
///
/// Never mutates the workspace TOML on disk; this is a per-invocation
/// command transform. Committed workspace files stay literal and
/// reproducible.
fn apply_resume_modifier(sess: &mut crate::domain::Session) -> Result<()> {
    use crate::domain::SessionKind;
    match sess.kind {
        Some(SessionKind::ClaudeCode) => {
            if !sess.command.contains("--continue") && !sess.command.contains("--resume") {
                sess.command.push_str(" --continue");
            }
        }
        Some(SessionKind::Opencode) => {
            // No stable resume flag we trust yet; surface honestly.
            eprintln!(
                "  --resume: no known resume flag for opencode kind yet; launching unchanged."
            );
        }
        _ => {
            eprintln!(
                "  --resume: session {:?} has no resumable kind (kind={:?}); launching unchanged.",
                sess.name, sess.kind
            );
        }
    }
    Ok(())
}

/// "Make this device the main session" — `pa claim`. Always uses
/// Takeover mode. Defaults the session name to the first one in the
/// workspace so the common case (only one agent-per-project) is a
/// single-arg command.
pub fn claim(
    session: Option<&str>,
    workspace: Option<&PathBuf>,
    dry_run: bool,
    resume: bool,
    fresh: bool,
) -> Result<()> {
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
    launch(
        name,
        workspace,
        dry_run,
        /* shared = */ false,
        resume,
        fresh,
        LaunchSupervisionOptions::default(),
    )
}

fn attach_mode_label(mode: AttachMode) -> &'static str {
    match mode {
        AttachMode::Takeover => "takeover: other clients will be detached",
        AttachMode::Shared => "shared: other clients stay attached",
    }
}

/// Quote a string as a TOML basic string (backslash-escape `\` and
/// `"`; nothing else needs escaping for the values we let users pass
/// on the command line). Used by both `init` and `add` when writing
/// TOML fragments.
fn toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Scaffold a new workspace file in the current directory. Writes
/// `<name>.portagenty.toml` with one starter session (`shell`, just
/// bash) so `pa` works end-to-end on the first run. Returns the path
/// that got written.
pub fn init(
    name: Option<String>,
    mpx: Option<InitMpxArg>,
    force: bool,
    with_agent_hooks: bool,
) -> Result<()> {
    use crate::scaffold::{create_at, ScaffoldOutcome};
    let cwd = std::env::current_dir().context("reading current directory")?;
    let workspace_name = match name {
        Some(n) => n,
        None => cwd
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("workspace")
            .to_string(),
    };
    // Resolution order: explicit --mpx wins, then the machine's
    // pinned default from $XDG_CONFIG_HOME/portagenty/config.toml,
    // then tmux as the last-resort fallback (matches the v1
    // reference adapter). The previous logic ignored the global
    // default — surprising for users who set zellij via the
    // onboarding wizard and then ran `pa init`.
    let mpx = match mpx {
        Some(InitMpxArg::Zellij) => MpxEnum::Zellij,
        Some(InitMpxArg::Tmux) => MpxEnum::Tmux,
        None => crate::config::current_default_multiplexer()
            .ok()
            .flatten()
            .unwrap_or(MpxEnum::Tmux),
    };

    let outcome = create_at(&cwd, &workspace_name, mpx, false, force)?;
    let out = io::stdout();
    let mut out = out.lock();
    match outcome {
        ScaffoldOutcome::Created(path) => {
            writeln!(out, "created {}", path.display())?;
            writeln!(
                out,
                "run `pa` here to open the TUI, or `pa add` to append more sessions"
            )?;
        }
        ScaffoldOutcome::AlreadyExisted(path) => {
            // With --with-agent-hooks on an existing workspace, the
            // intent is "retrofit agent hooks onto this project,"
            // not "clobber my TOML." Print a note and fall through
            // to the hook scaffold instead of bailing. Without the
            // hooks flag, keep the historical error so a bare
            // `pa init` in an already-scaffolded dir still fails
            // loudly.
            if with_agent_hooks {
                writeln!(
                    out,
                    "workspace file {} already exists — leaving it untouched; only scaffolding agent hooks.",
                    path.display()
                )?;
            } else {
                return Err(anyhow!(
                    "{} already exists; pass --force to overwrite",
                    path.display()
                ));
            }
        }
    }

    if with_agent_hooks {
        let report = scaffold_agent_hooks(&cwd)?;
        for line in report.lines() {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

/// Write `.mcp.json` + `.claude/commands/` + `.claude/skills/` at
/// `target_dir` so a Claude Code agent entering the workspace
/// self-discovers the portaconv (conversation extractor) integration
/// and portagenty's workspace shape. Files that already exist are
/// left alone — this is opt-in scaffolding, not authoritative
/// configuration, and users may have customized them.
///
/// Returns a multi-line report for the caller to print (which files
/// were created / skipped, plus a pconv-install hint if the binary
/// isn't on PATH). Errors only bubble up on filesystem I/O failure;
/// a missing pconv binary is a *hint*, not a failure — the hooks
/// are valid the moment pconv is installed.
pub(crate) fn scaffold_agent_hooks(target_dir: &std::path::Path) -> Result<String> {
    let mut report = String::new();
    report.push_str("scaffolding agent hooks:\n");

    let mcp_path = target_dir.join(".mcp.json");
    let cmd_dir = target_dir.join(".claude").join("commands");
    let skills_dir = target_dir.join(".claude").join("skills");

    std::fs::create_dir_all(&cmd_dir).with_context(|| format!("creating {}", cmd_dir.display()))?;
    std::fs::create_dir_all(&skills_dir)
        .with_context(|| format!("creating {}", skills_dir.display()))?;

    let files: [(&std::path::Path, &str, &str); 4] = [
        (mcp_path.as_path(), "  .mcp.json", MCP_JSON_TEMPLATE),
        (
            &cmd_dir.join("convos.md"),
            "  .claude/commands/convos.md",
            CONVOS_COMMAND_TEMPLATE,
        ),
        (
            &skills_dir.join("portaconv.md"),
            "  .claude/skills/portaconv.md",
            PORTACONV_SKILL_TEMPLATE,
        ),
        (
            &skills_dir.join("portagenty-workspace.md"),
            "  .claude/skills/portagenty-workspace.md",
            PORTAGENTY_WORKSPACE_SKILL_TEMPLATE,
        ),
    ];

    for (path, label, body) in files {
        if path.exists() {
            report.push_str(label);
            report.push_str("  (skipped — already exists)\n");
            continue;
        }
        std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
        report.push_str(label);
        report.push_str("  (created)\n");
    }

    // pconv detection: a portable `which`. ErrorKind::NotFound on a
    // --version spawn is the clearest signal that the binary is
    // absent. We don't want to fail init on a missing dep — just
    // hint.
    let pconv_present = std::process::Command::new("pconv")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    if !pconv_present {
        report.push_str(
            "\nhint: `pconv` (portaconv) is not on PATH yet. The scaffolded files\n\
             are harmless without it, but the Claude Code MCP handshake in .mcp.json\n\
             will only succeed once you run `cargo install portaconv` (or drop a\n\
             release binary onto PATH).\n",
        );
    }

    Ok(report)
}

/// `.mcp.json` template: points Claude Code at the locally-installed
/// pconv binary's stdio MCP server. Project-scoped MCP config —
/// applies only when the agent enters this workspace. `pconv mcp
/// serve` is portaconv's v0.1 MCP entrypoint (see portaconv/README).
const MCP_JSON_TEMPLATE: &str = r#"{
  "mcpServers": {
    "portaconv": {
      "command": "pconv",
      "args": ["mcp", "serve"]
    }
  }
}
"#;

/// A slash command template an agent can use to list conversations
/// via the `pa convos` shim — useful when the MCP handshake isn't
/// available (e.g. during cold-start inside a workspace). The YAML
/// frontmatter shape matches Claude Code's slash-command schema.
const CONVOS_COMMAND_TEMPLATE: &str = r#"---
description: List this workspace's prior Claude Code conversations.
---

Run `pa convos list` via the Bash tool and show the output.

Use `pa convos dump <id>` to export a specific conversation as
paste-ready markdown. Pass `--rewrite wsl-to-win` (or `win-to-wsl`)
to normalize OS-specific paths baked into content before pasting
into a new session.

`pa convos ...` auto-scopes to this workspace's TOML via
`--workspace-toml <path>`; you don't need to pass it yourself.
"#;

/// Skill describing what portaconv is and when to reach for it.
/// Skills are "the map of the tools you just installed" — per the
/// portaconv integration plan, the agent entering this workspace
/// should be able to self-discover the extraction capabilities
/// without the human explaining.
const PORTACONV_SKILL_TEMPLATE: &str = r#"---
name: portaconv
description: This workspace is wired to portaconv (`pconv`) — a terminal-native conversation extractor + MCP server for Claude Code history. Use it whenever the user asks to read, re-paste, or bridge prior-session context.
---

# portaconv

Portaconv (binary `pconv`) normalizes Claude Code's on-disk JSONL
conversation history into paste-ready markdown or MCP resources, and
rewrites WSL/Windows absolute paths baked into content so pasted
context lands coherently on whatever host is replying.

## Two interfaces, same data

- **MCP (preferred)** — `.mcp.json` registers the `portaconv` server.
  Tools: `list_conversations(since, workspace_id?, limit)` and
  `get_conversation(id, format, rewrite?)`. Each conversation is
  also exposed as a resource at `convos://conversation/<id>`.
- **CLI** — shell out via `pa convos ...` (a workspace-aware shim
  over `pconv`):
  - `pa convos list` — list conversations scoped to this workspace
  - `pa convos dump <id>` — emit paste-ready markdown
  - `pa convos dump <id> --rewrite wsl-to-win` — normalize paths

## Workspace scoping

`pa convos` injects `--workspace-toml <path>` so pconv sees only
conversations whose `cwd` prefix matches this workspace's
`projects` (plus any `previous_paths` — see the
`portagenty-workspace` skill).

## When to use

- User asks "what did we do before"/"show me the last session".
- User wants a transcript to paste into a new session / another
  agent CLI.
- Agent needs prior context to avoid re-asking the human.
- Folder was moved and old sessions aren't showing — check that
  `previous_paths` is populated in the workspace TOML (portagenty
  maintains this automatically on walk-up re-registration).
"#;

/// Skill describing portagenty's workspace shape so the agent knows
/// the structural contract of the TOML it's living in — session
/// list, multiplexer, stable `id`, `previous_paths`. Lets agents
/// read and reason about the workspace without the user having to
/// explain "what is a portagenty workspace."
const PORTAGENTY_WORKSPACE_SKILL_TEMPLATE: &str = r#"---
name: portagenty-workspace
description: This project is a portagenty workspace — a portable, terminal-native launcher for agent sessions, driven by a committed `*.portagenty.toml`. Read it to understand the workspace's declared sessions, multiplexer, project paths, and move history.
---

# portagenty-workspace

A portagenty workspace is a single TOML file at the project root
whose name ends in `.portagenty.toml`. It's the source of truth for
what sessions exist, what they run, and how they're multiplexed.

## Key fields

- `name` — human display name.
- `id` — UUID. Stable across folder moves; used by tooling
  (portaconv included) to track the workspace across environments.
- `multiplexer` — `"tmux"` or `"zellij"`.
- `projects = [...]` — project roots this workspace covers.
- `previous_paths = [...]` — auto-maintained by pa on walk-up
  re-registration. When the project folder moves, pa appends the
  old location here so external tools can bridge state
  (conversations, caches) authored at the old path.
- `[[session]]` blocks — `name`, `cwd`, `command`, optional
  `kind` (`claude-code`, `opencode`, `editor`, `dev-server`,
  `shell`, `other`) and `env`.

## Common commands

- `pa` — open the TUI (workspace picker → session list).
- `pa launch <name>` — attach a specific session.
- `pa claim <name>` — takeover: kicks other clients attached to
  the same session (the "move this session to this device" verb).
- `pa convos list` / `pa convos dump <id>` — see the `portaconv`
  skill.
- `pa add <name> --command "..."` — append a session without
  editing TOML by hand.

## Why sessions sometimes flash + exit

Multiplexers spawn commands under a *non-interactive* shell —
`~/.bashrc`/`~/.zshrc` aliases and functions are NOT loaded. A
`command = "my-alias"` that works in your interactive shell will
error with "command not found" inside pa. Write the literal command
or promote the alias to a real binary on PATH. See
<https://cybersader.github.io/portagenty/reference/schema/>
for the full story.
"#;

/// Append a new session to the current workspace file. Keeps the
/// existing content verbatim (comments + formatting preserved) —
/// just appends a `[[session]]` block at the end.
pub fn add(
    name: &str,
    command: &str,
    cwd: Option<&str>,
    kind: Option<AddKindArg>,
    description: Option<&str>,
    workspace: Option<&PathBuf>,
) -> Result<()> {
    // Find the workspace file.
    let ws_path = match workspace {
        Some(p) => p.clone(),
        None => crate::config::walk_up_from(
            &std::env::current_dir().context("reading current directory")?,
        )
        .ok_or_else(|| {
            anyhow!(
                "no *.portagenty.toml found walking up from {}. Run `pa init` here first.",
                std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "$PWD".into())
            )
        })?,
    };

    // Duplicate-check: load + parse to see if name already exists.
    // If so, bail clearly instead of producing a file with two
    // sessions of the same name (which load() would then error on).
    let existing: crate::config::WorkspaceFile = crate::config::load_toml(&ws_path)
        .with_context(|| format!("reading existing workspace file {}", ws_path.display()))?;
    if existing.sessions.iter().any(|s| s.name == name) {
        return Err(anyhow!(
            "session {name:?} already exists in {}. Delete it by hand or pick a different name.",
            ws_path.display(),
        ));
    }

    let cwd_val = cwd.unwrap_or(".");

    let mut block = String::new();
    block.push_str("\n[[session]]\n");
    block.push_str(&format!("name = {}\n", toml_basic_string(name)));
    block.push_str(&format!("cwd = {}\n", toml_basic_string(cwd_val)));
    block.push_str(&format!("command = {}\n", toml_basic_string(command)));
    if let Some(k) = kind {
        let kind_str = match crate::domain::SessionKind::from(k) {
            crate::domain::SessionKind::ClaudeCode => "claude-code",
            crate::domain::SessionKind::Opencode => "opencode",
            crate::domain::SessionKind::Editor => "editor",
            crate::domain::SessionKind::DevServer => "dev-server",
            crate::domain::SessionKind::Shell => "shell",
            crate::domain::SessionKind::Other => "other",
        };
        block.push_str(&format!("kind = \"{kind_str}\"\n"));
    }
    if let Some(desc) = description.filter(|d| !d.is_empty()) {
        block.push_str(&format!("description = {}\n", toml_basic_string(desc)));
    }

    // Read existing contents so we preserve everything (comments,
    // whitespace, trailing-newline decisions). Append the new block.
    let mut contents = std::fs::read_to_string(&ws_path)
        .with_context(|| format!("reading {}", ws_path.display()))?;
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(&block);

    std::fs::write(&ws_path, contents).with_context(|| format!("writing {}", ws_path.display()))?;

    let out = io::stdout();
    let mut out = out.lock();
    writeln!(out, "added session {name:?} to {}", ws_path.display())?;
    Ok(())
}

pub fn onboard() -> Result<()> {
    crate::onboarding::run_wizard(true)?;
    Ok(())
}

/// Emit a shell completion script to stdout. Covers every
/// subcommand and flag clap knows about. Dynamic completions
/// (session names, snippet names) are not included in v1.x — those
/// land in a follow-up.
pub fn completions(shell: clap_complete::Shell) -> Result<()> {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    clap_complete::generate(shell, &mut cmd, bin_name, &mut out);
    Ok(())
}

/// Find the current workspace file (walk-up or explicit path).
fn resolve_workspace_path(workspace: Option<&PathBuf>) -> Result<PathBuf> {
    match workspace {
        Some(p) => Ok(p.clone()),
        None => crate::config::walk_up_from(
            &std::env::current_dir().context("reading current directory")?,
        )
        .ok_or_else(|| {
            anyhow!(
                "no *.portagenty.toml found walking up from {}. Run `pa init` here first.",
                std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "$PWD".into())
            )
        }),
    }
}

pub fn rm(name: &str, workspace: Option<&PathBuf>) -> Result<()> {
    let path = resolve_workspace_path(workspace)?;
    remove_session_from_file(&path, name)?;
    let out = io::stdout();
    let mut out = out.lock();
    writeln!(out, "removed session {name:?} from {}", path.display())?;
    Ok(())
}

/// Pure file-mutation core of `pa rm`. No stdio — callable from the
/// TUI's row-delete action. Preserves comments and formatting via
/// toml_edit; errors if the session name isn't present, with a helpful
/// list of available names. Exposed to the TUI via
/// `pub(crate)` so cross-module callers don't reach into CLI-private
/// internals.
pub(crate) fn remove_session_from_file(path: &std::path::Path, name: &str) -> Result<()> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;

    let array = doc
        .get_mut("session")
        .and_then(|v| v.as_array_of_tables_mut())
        .ok_or_else(|| anyhow!("workspace {} has no sessions to remove", path.display()))?;

    let idx = array
        .iter()
        .position(|t| {
            t.get("name")
                .and_then(|v| v.as_str())
                .map(|n| n == name)
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            let available: Vec<String> = array
                .iter()
                .filter_map(|t| {
                    t.get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .collect();
            anyhow!(
                "no session named {name:?} in {}. available: {}",
                path.display(),
                if available.is_empty() {
                    "(none)".into()
                } else {
                    available.join(", ")
                }
            )
        })?;

    array.remove(idx);

    std::fs::write(path, doc.to_string()).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn edit(
    name: &str,
    command: Option<&str>,
    cwd: Option<&str>,
    kind: Option<AddKindArg>,
    rename: Option<&str>,
    description: Option<&str>,
    env_set: &[String],
    env_unset: &[String],
    workspace: Option<&PathBuf>,
) -> Result<()> {
    // Of the field-replacement flags (command/cwd/kind/rename/
    // description) at most one can apply per invocation — picking
    // which TOML field got the user's intent shouldn't be a guessing
    // game. env-set and env-unset are independent and stack freely
    // with each other and with one field flag.
    let field_flags = [
        command.is_some(),
        cwd.is_some(),
        kind.is_some(),
        rename.is_some(),
        description.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if field_flags > 1 {
        return Err(anyhow!(
            "pa edit takes at most one of --command / --cwd / --kind / --rename / --description \
             per call (use additional --env / --unset-env alongside them as needed)"
        ));
    }
    if field_flags == 0 && env_set.is_empty() && env_unset.is_empty() {
        return Err(anyhow!(
            "pa edit needs at least one of --command / --cwd / --kind / --rename / --description / --env / --unset-env"
        ));
    }

    // Validate KEY=VAL parsing up front so a malformed --env aborts
    // before we touch the file.
    let env_pairs: Vec<(String, String)> = env_set
        .iter()
        .map(|s| parse_env_kv(s))
        .collect::<Result<_>>()?;

    let path = resolve_workspace_path(workspace)?;
    let op = EditOp {
        command: command.map(str::to_string),
        cwd: cwd.map(str::to_string),
        kind: kind.map(crate::domain::SessionKind::from),
        rename: rename.map(str::to_string),
        description: description.map(str::to_string),
        env_set: env_pairs,
        env_unset: env_unset.to_vec(),
    };

    edit_session_in_file(&path, name, &op)?;

    let out = io::stdout();
    let mut out = out.lock();
    writeln!(out, "edited session {name:?} in {}", path.display())?;
    Ok(())
}

/// Bundle of changes to apply to a single session row in a workspace
/// file. Pure data; no I/O. Used by both the CLI `pa edit` path and
/// (forthcoming) the in-TUI `e` field-edit flow.
#[derive(Debug, Clone, Default)]
pub struct EditOp {
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub kind: Option<crate::domain::SessionKind>,
    pub rename: Option<String>,
    /// New description. `Some("")` clears it (removes the key);
    /// `Some(text)` sets it; `None` leaves it untouched.
    pub description: Option<String>,
    /// Env entries to set/overwrite. Order doesn't matter; the on-
    /// disk env table is a TOML map.
    pub env_set: Vec<(String, String)>,
    /// Env keys to remove. Silent no-op for missing keys.
    pub env_unset: Vec<String>,
}

/// Apply `op` to the named session inside the workspace file at
/// `path`. Preserves comments + formatting via toml_edit; errors
/// surface cleanly with the file path attached. Pub(crate) so the
/// TUI's `e`-key flow can call without going through the CLI dispatch.
pub(crate) fn edit_session_in_file(path: &std::path::Path, name: &str, op: &EditOp) -> Result<()> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;

    // Collect sibling names BEFORE we take a mutable handle to the
    // target table, to avoid overlapping borrows.
    let sibling_names: Vec<String> = doc
        .get("session")
        .and_then(|v| v.as_array_of_tables())
        .into_iter()
        .flat_map(|a| a.iter())
        .filter_map(|t| {
            let tname = t
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            // Exclude the target session itself so a no-op rename
            // doesn't collide with its own name.
            match tname {
                Some(n) if n != name => Some(n),
                _ => None,
            }
        })
        .collect();

    if let Some(new_name) = &op.rename {
        if sibling_names.iter().any(|n| n == new_name) {
            return Err(anyhow!(
                "another session is already named {new_name:?} in {}",
                path.display()
            ));
        }
    }

    let array = doc
        .get_mut("session")
        .and_then(|v| v.as_array_of_tables_mut())
        .ok_or_else(|| anyhow!("workspace {} has no sessions to edit", path.display()))?;

    let table = array
        .iter_mut()
        .find(|t| {
            t.get("name")
                .and_then(|v| v.as_str())
                .map(|n| n == name)
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow!("no session named {name:?} in {}", path.display()))?;

    if let Some(new_cmd) = &op.command {
        table["command"] = toml_edit::value(new_cmd.as_str());
    }
    if let Some(new_cwd) = &op.cwd {
        table["cwd"] = toml_edit::value(new_cwd.as_str());
    }
    if let Some(k) = op.kind {
        let kind_str = match k {
            crate::domain::SessionKind::ClaudeCode => "claude-code",
            crate::domain::SessionKind::Opencode => "opencode",
            crate::domain::SessionKind::Editor => "editor",
            crate::domain::SessionKind::DevServer => "dev-server",
            crate::domain::SessionKind::Shell => "shell",
            crate::domain::SessionKind::Other => "other",
        };
        table["kind"] = toml_edit::value(kind_str);
    }
    if let Some(new_name) = &op.rename {
        table["name"] = toml_edit::value(new_name.as_str());
    }
    if let Some(desc) = &op.description {
        // Empty string clears the note (drops the key); any other
        // value sets it.
        if desc.is_empty() {
            table.remove("description");
        } else {
            table["description"] = toml_edit::value(desc.as_str());
        }
    }

    // env: applied AFTER the field changes so unset/set are
    // visible in the same TOML write.
    if !op.env_set.is_empty() || !op.env_unset.is_empty() {
        // Ensure an `env` inline-or-table exists. Prefer regular
        // table syntax for legibility on non-trivial env lists.
        if !table.contains_key("env") {
            table.insert("env", toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let env_item = table
            .get_mut("env")
            .ok_or_else(|| anyhow!("env table missing after insert"))?;
        let env_table = env_item
            .as_table_mut()
            .ok_or_else(|| anyhow!("env field is not a table in {}", path.display()))?;
        for k in &op.env_unset {
            env_table.remove(k);
        }
        for (k, v) in &op.env_set {
            env_table[k.as_str()] = toml_edit::value(v.as_str());
        }
        // If env is now empty, drop the key entirely so the file
        // stays tidy.
        if env_table.is_empty() {
            table.remove("env");
        }
    }

    std::fs::write(path, doc.to_string()).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Parse a `KEY=VAL` pair from a single CLI flag value. Errors when
/// the `=` separator is missing or the key portion is empty.
fn parse_env_kv(raw: &str) -> Result<(String, String)> {
    let (k, v) = raw
        .split_once('=')
        .ok_or_else(|| anyhow!("expected KEY=VAL, got {raw:?}"))?;
    if k.is_empty() {
        return Err(anyhow!("env key cannot be empty in {raw:?}"));
    }
    Ok((k.to_string(), v.to_string()))
}

pub fn snippets(cmd: SnippetsCommand) -> Result<()> {
    use crate::snippets as sn;
    let out = io::stdout();
    let mut out = out.lock();
    match cmd {
        SnippetsCommand::List => {
            writeln!(out, "bundled pa snippets:")?;
            for s in sn::SNIPPETS {
                writeln!(out, "  {:<20}  {}", s.name, s.description)?;
            }
            writeln!(
                out,
                "\nInstall one with: pa snippets install <name>  (default target: ~/.bashrc)"
            )?;
        }
        SnippetsCommand::Show { name } => {
            let s = sn::lookup(&name)?;
            writeln!(out, "# {} — {}", s.name, s.description)?;
            out.write_all(s.contents.as_bytes())?;
        }
        SnippetsCommand::Install { name, to, dry_run } => {
            let s = sn::lookup(&name)?;
            let target = match to {
                Some(p) => p,
                None => sn::default_rcfile()?,
            };
            if dry_run {
                let existing = std::fs::read_to_string(&target).unwrap_or_default();
                let new = sn::install_into(&existing, s);
                writeln!(
                    out,
                    "# DRY RUN: would write the following to {}:",
                    target.display()
                )?;
                out.write_all(new.as_bytes())?;
            } else {
                sn::install(&target, s)?;
                writeln!(
                    out,
                    "installed snippet {:?} into {}",
                    s.name,
                    target.display()
                )?;
                writeln!(
                    out,
                    "reload your shell or `source {}` to pick up the changes.",
                    target.display()
                )?;
            }
        }
        SnippetsCommand::Uninstall {
            name,
            from,
            dry_run,
        } => {
            let target = match from {
                Some(p) => p,
                None => sn::default_rcfile()?,
            };
            if dry_run {
                let existing = std::fs::read_to_string(&target).unwrap_or_default();
                match sn::uninstall_from(&existing, &name) {
                    Some(new) => {
                        writeln!(
                            out,
                            "# DRY RUN: would write the following to {} (snippet {:?} removed):",
                            target.display(),
                            name
                        )?;
                        out.write_all(new.as_bytes())?;
                    }
                    None => writeln!(
                        out,
                        "snippet {name:?} is not installed in {}",
                        target.display()
                    )?,
                }
            } else {
                match sn::uninstall(&target, &name)? {
                    Some(_) => writeln!(out, "removed snippet {name:?} from {}", target.display())?,
                    None => writeln!(
                        out,
                        "snippet {name:?} was not installed in {}",
                        target.display()
                    )?,
                }
            }
        }
    }
    Ok(())
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

// ─── pa convos ──────────────────────────────────────────────────────────

/// Forward args to `pconv` with this workspace's TOML prepended as
/// `--workspace-toml <path>`. Thin pass-through: portagenty does NOT
/// parse pconv's subcommand surface — any subcommand, flag, or
/// shape pconv supports now or in the future works verbatim via
/// `pa convos ...`.
///
/// Design notes:
///   - Workspace resolution: explicit `-w` > walk-up from `$PWD`.
///     If neither resolves, error early with a clear hint (we don't
///     want pconv to handle "no workspace" — that's a portagenty
///     concern).
///   - `--workspace-toml` injection is skipped when the caller
///     already passed it (avoids `pconv: unexpected argument`).
///   - Missing pconv binary is a portagenty-owned error with an
///     install hint. We check via `std::process::Command` spawn
///     returning `ErrorKind::NotFound`, matching how `cargo` /
///     `git` shell out to find-or-hint-install other tools.
///   - stdin / stdout / stderr pass through unchanged via `status()`
///     so the user's terminal sees pconv output directly (no capture,
///     no reformatting). Exit code propagates via `std::process::exit`
///     on any non-zero status — scripts relying on `$?` still work.
pub fn convos(workspace: Option<&PathBuf>, args: &[String]) -> Result<()> {
    let ws_path = resolve_workspace_path(workspace)?;
    let pconv_args = build_pconv_argv(args, &ws_path);

    let status = match std::process::Command::new("pconv")
        .args(&pconv_args)
        .status()
    {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!(
                "`pconv` not found on PATH. portaconv is a separate crate — \
                 install it with `cargo install portaconv` \
                 (or grab a release from https://github.com/cybersader/portaconv). \
                 portagenty stays agnostic of the agent-CLI you use; `pa convos` \
                 is just a workspace-aware shim."
            ));
        }
        Err(err) => {
            return Err(
                anyhow::Error::from(err).context("spawning `pconv` (portaconv) from `pa convos`")
            );
        }
    };

    if !status.success() {
        // Exit with pconv's code so scripts (`pa convos list && ...`)
        // see the same failure signal they'd see from raw pconv.
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Build the argv to pass to `pconv`. Pure function, split out so
/// arg-ordering is regression-testable without shelling out.
///
/// Auto-injects `--workspace-toml <ws_path>` AFTER the first
/// positional arg (the pconv subcommand). pconv's `--workspace-toml`
/// is a subcommand-level flag (parsed by `list` / `dump`), so
/// injecting it before the subcommand name — as a naive "prepend to
/// argv" would — trips pconv's top-level clap parser with
/// `unexpected argument '--workspace-toml'`.
///
/// Skipped when:
///   - the caller already passed `--workspace-toml` (respect intent)
///   - the caller ran `pa convos` bare or only with flags / `--help`
///     (no subcommand → let pconv surface its own help)
fn build_pconv_argv(args: &[String], ws_path: &std::path::Path) -> Vec<String> {
    let caller_passed_ws_flag = args
        .iter()
        .any(|a| a == "--workspace-toml" || a.starts_with("--workspace-toml="));

    if caller_passed_ws_flag {
        return args.to_vec();
    }

    let Some(subcmd_idx) = args.iter().position(|a| !a.starts_with('-')) else {
        return args.to_vec();
    };

    let mut out = Vec::with_capacity(args.len() + 2);
    out.extend(args[..=subcmd_idx].iter().cloned());
    out.push("--workspace-toml".to_string());
    out.push(ws_path.display().to_string());
    out.extend(args[subcmd_idx + 1..].iter().cloned());
    out
}

// ─── pa open <url> ──────────────────────────────────────────────────────

/// Dispatch a `pa://...` URL to the matching pa action. Entry point
/// for the OS-level URL scheme handler installed by `pa protocol
/// install`. Printing a usable error on bad URLs beats silently
/// opening the picker — URL clicks are asynchronous, the user might
/// not see the terminal window that was spawned.
pub fn open_url(url: &str) -> Result<()> {
    use crate::protocol::ProtocolAction;
    match crate::protocol::parse(url)? {
        ProtocolAction::Open(path) => crate::tui::run(Some(&path)),
        ProtocolAction::Shell(path) => {
            // Re-use the same shell-out path the TUI uses. Print a
            // banner first so the user knows why pa didn't launch.
            eprintln!();
            eprintln!("  pa → shell at {}", path.display());
            eprintln!("        (from pa://shell URL click)");
            eprintln!();
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
            let _ = std::process::Command::new(&shell)
                .current_dir(&path)
                .status()
                .with_context(|| format!("spawning shell at {}", path.display()))?;
            Ok(())
        }
        ProtocolAction::WorkspaceById(id) => {
            let path = resolve_workspace_by_id(&id)?;
            crate::tui::run(Some(&path))
        }
        ProtocolAction::LaunchSession {
            workspace_id,
            session,
        } => {
            let path = resolve_workspace_by_id(&workspace_id)?;
            launch(
                &session,
                Some(&path),
                false,
                false,
                false,
                false,
                LaunchSupervisionOptions::default(),
            )
        }
    }
}

/// Scan the global workspace registry for a TOML with `id =
/// "<uuid>"` and return its file path. Errors if no match.
fn resolve_workspace_by_id(id: &str) -> Result<PathBuf> {
    for ws_path in crate::config::list_registered_workspaces().unwrap_or_default() {
        let raw = match std::fs::read_to_string(&ws_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let doc: toml_edit::DocumentMut = match raw.parse() {
            Ok(d) => d,
            Err(_) => continue,
        };
        if let Some(got) = doc.get("id").and_then(|v| v.as_str()) {
            if got == id {
                return Ok(ws_path);
            }
        }
    }
    Err(anyhow!(
        "no registered workspace has id {id:?}. Make sure the workspace TOML has an `id = \"...\"` field and is in the global registry."
    ))
}

// ─── pa protocol ... ────────────────────────────────────────────────────

/// `pa protocol terminals` — list detected emulators for the current OS.
pub fn protocol_terminals() -> Result<()> {
    let terms = crate::protocol::register::detect_terminals();
    if terms.is_empty() {
        println!(
            "No terminal emulators detected on {}.",
            std::env::consts::OS
        );
        println!(
            "Pass --terminal <name> on install/show to pick any binary, or install\n\
             one of: wt.exe, alacritty, kitty, wezterm, gnome-terminal, konsole, ..."
        );
        return Ok(());
    }
    println!("Detected terminals (first entry is the install default):");
    for t in &terms {
        println!("  {t}");
    }
    Ok(())
}

fn pick_terminal(override_name: Option<&str>) -> Result<crate::protocol::register::Terminal> {
    let terms = crate::protocol::register::detect_terminals();
    if let Some(name) = override_name {
        // 1) Name match against detected set (case-insensitive, substring).
        if let Some(t) = crate::protocol::register::match_by_name(&terms, name) {
            return Ok(t);
        }
        // 2) Custom: treat `name` as a binary path or PATH-resolvable
        //    command. Works even when we didn't detect it. The user
        //    vouches for the binary; we construct a generic
        //    `-e {cmd}` template that most emulators accept.
        if let Some(custom) = crate::protocol::register::custom_terminal(name) {
            return Ok(custom);
        }
        let avail: Vec<String> = terms.iter().map(|t| t.name.clone()).collect();
        Err(anyhow!(
            "terminal {name:?} not found. Detected: {}. \
             You can also pass an absolute path to any terminal binary.",
            if avail.is_empty() {
                "(none — run `pa protocol terminals`)".into()
            } else {
                avail.join(", ")
            }
        ))
    } else {
        terms.into_iter().next().ok_or_else(|| {
            anyhow!(
                "no terminal emulator detected — pass --terminal <name-or-path>. \
                 Run `pa protocol terminals` for the list we probe for."
            )
        })
    }
}

fn own_binary_path() -> Result<PathBuf> {
    std::env::current_exe().context("reading current executable path")
}

/// `pa protocol show [--terminal ...]` — print registration snippet.
pub fn protocol_show(terminal: Option<&str>) -> Result<()> {
    let term = pick_terminal(terminal)?;
    let bin = own_binary_path()?;
    let snippet = crate::protocol::register::show_snippet(&term, &bin)?;
    eprintln!("# Using terminal: {term}");
    eprintln!("# pa binary: {}", bin.display());
    eprintln!();
    println!("{snippet}");
    Ok(())
}

/// `pa protocol install [--terminal ...]` — write the registration.
pub fn protocol_install(terminal: Option<&str>) -> Result<()> {
    let term = pick_terminal(terminal)?;
    let bin = own_binary_path()?;
    let where_to = crate::protocol::register::install(&term, &bin)?;
    println!("installed pa:// handler via {}\n  → {}", term, where_to);
    println!(
        "\nTry: click a pa://open/<url-encoded-absolute-path> link, or run\n  xdg-open 'pa://open/tmp' (Linux)  /  start pa://open/tmp (Windows)"
    );
    Ok(())
}

/// `pa protocol uninstall` — reverse of install.
pub fn protocol_uninstall() -> Result<()> {
    let where_from = crate::protocol::register::uninstall()?;
    println!("uninstalled pa:// handler\n  → {}", where_from);
    Ok(())
}

/// `pa protocol status` — print what's currently registered.
pub fn protocol_status() -> Result<()> {
    let s = crate::protocol::register::status()?;
    print!("{s}");
    Ok(())
}

#[cfg(test)]
mod return_banner_tests {
    use super::*;

    #[test]
    fn return_banner_uses_human_workspace_and_session_identity() {
        assert_eq!(
            return_banner("21 - Teaching", "shell"),
            "pa ← returned from \"21 - Teaching / shell\""
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_replacement_limits_use_current_kind_policy_before_cleanup() {
        let legacy_claude = ResourceLimits {
            memory_high_bytes: Some(12 * ResourceLimits::GIB),
            memory_max_bytes: Some(12 * ResourceLimits::GIB),
            memory_swap_max_bytes: None,
            cpu_quota_percent: None,
            tasks_max: None,
        };
        assert_eq!(
            effective_stale_replacement_limits(
                Some(crate::domain::SessionKind::ClaudeCode),
                true,
                legacy_claude,
            )
            .unwrap(),
            ResourceLimits::claude_defaults()
        );
        assert_eq!(
            effective_stale_replacement_limits(
                Some(crate::domain::SessionKind::Shell),
                true,
                ResourceLimits::claude_defaults(),
            )
            .unwrap(),
            ResourceLimits::default()
        );
    }

    #[test]
    fn client_exit_message_preserves_code_and_signal() {
        assert_eq!(
            client_exit_message(ClientExit {
                code: Some(7),
                signal: None,
            }),
            "multiplexer client exited abnormally with code 7"
        );
        assert_eq!(
            client_exit_message(ClientExit {
                code: None,
                signal: Some(15),
            }),
            "multiplexer client was terminated by signal 15"
        );
    }
}

#[cfg(test)]
mod convos_argv_tests {
    //! Guard against the "--workspace-toml landed in the wrong slot"
    //! regression discovered during end-to-end verification with a
    //! real `pconv` binary. `pconv`'s --workspace-toml is a subcommand-
    //! level flag (parsed by `list` / `dump`), not global — so it
    //! MUST appear after the subcommand name in argv.
    use super::*;

    fn s(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn injects_workspace_toml_after_subcommand() {
        let ws = std::path::Path::new("/ws/my.portagenty.toml");
        let out = build_pconv_argv(&[s("list")], ws);
        assert_eq!(
            out,
            vec![
                s("list"),
                s("--workspace-toml"),
                s("/ws/my.portagenty.toml"),
            ]
        );
    }

    #[test]
    fn injects_before_caller_flags_but_after_subcommand() {
        let ws = std::path::Path::new("/ws/my.portagenty.toml");
        let out = build_pconv_argv(&[s("list"), s("--since"), s("7d")], ws);
        assert_eq!(
            out,
            vec![
                s("list"),
                s("--workspace-toml"),
                s("/ws/my.portagenty.toml"),
                s("--since"),
                s("7d"),
            ]
        );
    }

    #[test]
    fn injects_after_subcommand_when_positional_id_follows() {
        let ws = std::path::Path::new("/ws/my.portagenty.toml");
        let out = build_pconv_argv(&[s("dump"), s("abc-session-id")], ws);
        assert_eq!(
            out,
            vec![
                s("dump"),
                s("--workspace-toml"),
                s("/ws/my.portagenty.toml"),
                s("abc-session-id"),
            ]
        );
    }

    #[test]
    fn respects_caller_supplied_workspace_toml_flag() {
        let ws = std::path::Path::new("/ws/my.portagenty.toml");
        let caller = vec![s("list"), s("--workspace-toml"), s("/other/path")];
        let out = build_pconv_argv(&caller, ws);
        assert_eq!(out, caller, "should pass through without duplicate");
    }

    #[test]
    fn respects_caller_supplied_workspace_toml_equals_form() {
        let ws = std::path::Path::new("/ws/my.portagenty.toml");
        let caller = vec![s("list"), s("--workspace-toml=/other/path")];
        let out = build_pconv_argv(&caller, ws);
        assert_eq!(out, caller);
    }

    #[test]
    fn skips_injection_when_no_subcommand() {
        // `pa convos` bare, or `pa convos --help` — no positional.
        // Let pconv surface its own help/error; don't force a flag
        // into an empty or help-only call.
        let ws = std::path::Path::new("/ws/my.portagenty.toml");
        assert_eq!(build_pconv_argv(&[], ws), Vec::<String>::new());
        let help = vec![s("--help")];
        assert_eq!(build_pconv_argv(&help, ws), help);
    }
}

#[cfg(test)]
mod agent_hooks_tests {
    use super::*;

    #[test]
    fn scaffold_writes_expected_files() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let report = scaffold_agent_hooks(tmp.path()).unwrap();
        assert!(report.contains(".mcp.json"));
        assert!(report.contains("portaconv.md"));
        assert!(report.contains("portagenty-workspace.md"));
        assert!(report.contains("convos.md"));

        assert!(tmp.path().join(".mcp.json").is_file());
        assert!(tmp.path().join(".claude/commands/convos.md").is_file());
        assert!(tmp.path().join(".claude/skills/portaconv.md").is_file());
        assert!(tmp
            .path()
            .join(".claude/skills/portagenty-workspace.md")
            .is_file());

        let mcp = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
        assert!(mcp.contains(r#""pconv""#), "mcp.json should register pconv");
        assert!(mcp.contains(r#""mcp""#));
        assert!(mcp.contains(r#""serve""#));
    }

    #[test]
    fn scaffold_does_not_overwrite_existing_files() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mcp_path = tmp.path().join(".mcp.json");
        std::fs::write(&mcp_path, r#"{"mcpServers":{"custom":{}}}"#).unwrap();
        let report = scaffold_agent_hooks(tmp.path()).unwrap();
        assert!(
            report.contains(".mcp.json") && report.contains("skipped"),
            "existing .mcp.json should be skipped: {report}"
        );
        let preserved = std::fs::read_to_string(&mcp_path).unwrap();
        assert!(
            preserved.contains("custom"),
            "user's .mcp.json was overwritten: {preserved}"
        );
    }

    #[test]
    fn scaffold_creates_nested_dirs_when_absent() {
        // Scaffold on a brand-new dir with no `.claude/` at all.
        let tmp = assert_fs::TempDir::new().unwrap();
        assert!(!tmp.path().join(".claude").exists());
        scaffold_agent_hooks(tmp.path()).unwrap();
        assert!(tmp.path().join(".claude/commands").is_dir());
        assert!(tmp.path().join(".claude/skills").is_dir());
    }
}
