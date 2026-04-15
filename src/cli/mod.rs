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

        /// Resume prior state for kind-aware sessions. For
        /// `kind = "claude-code"` this appends `--continue` to the
        /// command string before launch. Silent no-op with a hint on
        /// other kinds — workspace TOML command strings stay literal
        /// so committed workspace files are reproducible.
        #[arg(long = "resume")]
        resume: bool,
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
    /// Change one field on an existing session without opening an
    /// editor. Pass exactly one of --command / --cwd / --kind /
    /// --rename; comments and formatting elsewhere in the file stay
    /// untouched.
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
    launch(name, workspace, dry_run, /* shared = */ false, resume)
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
    let cwd = std::env::current_dir().context("reading current directory")?;
    let workspace_name = match name {
        Some(n) => n,
        None => cwd
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("workspace")
            .to_string(),
    };

    // Sanitize for filename use: replace anything non-safe with `_`.
    // Filenames are strict about what's tolerable; the on-disk name
    // is separate from the display name.
    let filename_stem: String = workspace_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let filename = format!("{filename_stem}.portagenty.toml");
    let path = cwd.join(&filename);

    if path.exists() && !force {
        return Err(anyhow!(
            "{} already exists; pass --force to overwrite",
            path.display()
        ));
    }

    let mpx = match mpx {
        Some(InitMpxArg::Zellij) => "zellij",
        Some(InitMpxArg::Tmux) | None => "tmux",
    };

    let contents = format!(
        r#"# Workspace file for portagenty. See:
# https://cybersader.github.io/portagenty/reference/schema/
name = {name}
multiplexer = "{mpx}"

[[session]]
name = "shell"
cwd = "."
command = "bash"
kind = "shell"
"#,
        name = toml_basic_string(&workspace_name),
    );

    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;

    // Register globally so `pa` can list this workspace from any
    // directory without relying on walk-up. Best-effort.
    let _ = crate::config::register_global_workspace(&path);

    let out = io::stdout();
    let mut out = out.lock();
    writeln!(out, "created {}", path.display())?;
    writeln!(
        out,
        "run `pa` here to open the TUI, or `pa add` to append more sessions"
    )?;
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
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;

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

    std::fs::write(&path, doc.to_string())
        .with_context(|| format!("writing {}", path.display()))?;

    let out = io::stdout();
    let mut out = out.lock();
    writeln!(out, "removed session {name:?} from {}", path.display())?;
    Ok(())
}

pub fn edit(
    name: &str,
    command: Option<&str>,
    cwd: Option<&str>,
    kind: Option<AddKindArg>,
    rename: Option<&str>,
    workspace: Option<&PathBuf>,
) -> Result<()> {
    // Require exactly one change — otherwise it's ambiguous what
    // the user meant. Rather than apply all of a multi-flag set,
    // error out with guidance.
    let flag_count = [
        command.is_some(),
        cwd.is_some(),
        kind.is_some(),
        rename.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if flag_count == 0 {
        return Err(anyhow!(
            "pa edit needs one of --command, --cwd, --kind, --rename"
        ));
    }
    if flag_count > 1 {
        return Err(anyhow!(
            "pa edit takes exactly one change at a time — pass only one of --command / --cwd / --kind / --rename"
        ));
    }

    let path = resolve_workspace_path(workspace)?;
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;

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

    // Check the rename target doesn't collide with any other session.
    if let Some(new_name) = rename {
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

    if let Some(new_cmd) = command {
        table["command"] = toml_edit::value(new_cmd);
    }
    if let Some(new_cwd) = cwd {
        table["cwd"] = toml_edit::value(new_cwd);
    }
    if let Some(k) = kind {
        let kind_str = match crate::domain::SessionKind::from(k) {
            crate::domain::SessionKind::ClaudeCode => "claude-code",
            crate::domain::SessionKind::Opencode => "opencode",
            crate::domain::SessionKind::Editor => "editor",
            crate::domain::SessionKind::DevServer => "dev-server",
            crate::domain::SessionKind::Shell => "shell",
            crate::domain::SessionKind::Other => "other",
        };
        table["kind"] = toml_edit::value(kind_str);
    }
    if let Some(new_name) = rename {
        table["name"] = toml_edit::value(new_name);
    }

    std::fs::write(&path, doc.to_string())
        .with_context(|| format!("writing {}", path.display()))?;

    let out = io::stdout();
    let mut out = out.lock();
    writeln!(out, "edited session {name:?} in {}", path.display())?;
    Ok(())
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
