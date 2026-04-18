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
    /// most one of --command / --cwd / --kind / --rename per call;
    /// --env KEY=VAL and --unset-env KEY are repeatable and stack
    /// freely with each other and with one field flag. Comments
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

    mux.create_and_attach(&sess, &mpx_name, mode)
        .with_context(|| format!("launching session {:?}", sess.name))
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
    launch(name, workspace, dry_run, /* shared = */ false, resume, fresh)
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
pub fn init(name: Option<String>, mpx: Option<InitMpxArg>, force: bool) -> Result<()> {
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
            return Err(anyhow!(
                "{} already exists; pass --force to overwrite",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Append a new session to the current workspace file. Keeps the
/// existing content verbatim (comments + formatting preserved) —
/// just appends a `[[session]]` block at the end.
pub fn add(
    name: &str,
    command: &str,
    cwd: Option<&str>,
    kind: Option<AddKindArg>,
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
    env_set: &[String],
    env_unset: &[String],
    workspace: Option<&PathBuf>,
) -> Result<()> {
    // Of the field-replacement flags (command/cwd/kind/rename) at
    // most one can apply per invocation — picking which TOML field
    // got the user's intent shouldn't be a guessing game. env-set
    // and env-unset are independent and stack freely with each
    // other and with one field flag.
    let field_flags = [
        command.is_some(),
        cwd.is_some(),
        kind.is_some(),
        rename.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if field_flags > 1 {
        return Err(anyhow!(
            "pa edit takes at most one of --command / --cwd / --kind / --rename per call \
             (use additional --env / --unset-env alongside them as needed)"
        ));
    }
    if field_flags == 0 && env_set.is_empty() && env_unset.is_empty() {
        return Err(anyhow!(
            "pa edit needs at least one of --command / --cwd / --kind / --rename / --env / --unset-env"
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
            launch(&session, Some(&path), false, false, false, false)
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
        println!("No terminal emulators detected on {}.", std::env::consts::OS);
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

fn pick_terminal(
    override_name: Option<&str>,
) -> Result<crate::protocol::register::Terminal> {
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
        terms
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!(
                "no terminal emulator detected — pass --terminal <name-or-path>. \
                 Run `pa protocol terminals` for the list we probe for."
            ))
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
    println!(
        "installed pa:// handler via {}\n  → {}",
        term, where_to
    );
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
