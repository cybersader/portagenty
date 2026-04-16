//! zellij adapter. Shells out to the `zellij` CLI via `std::process::Command`.
//!
//! Differences from the tmux adapter worth knowing about:
//!
//! - **No nested sessions.** zellij refuses to start or attach to a
//!   session from inside an existing zellij client. The adapter
//!   detects `ZELLIJ_SESSION_NAME` + `ZELLIJ` env vars at runtime and
//!   returns an actionable error instead of the opaque one zellij
//!   emits on its own.
//! - **No per-session cwd exposed.** `list_sessions` returns names
//!   only; `SessionInfo::cwd` is always `None` for zellij.
//! - **No CLI detach action.** `detach_current` returns an error
//!   directing the user to the multiplexer's keybind (Ctrl+Q by
//!   default). Parity with tmux's `detach-client` is fundamentally
//!   not available here.
//! - **Sessions with a command** are spawned via a generated KDL
//!   layout file (see `write_layout_file`); zellij's `attach
//!   --create` alone doesn't accept a pane command, so we hand it a
//!   layout that does.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Result};

use crate::domain::Session;
use crate::mux::{AttachMode, Multiplexer, SessionInfo};

/// zellij-backed [`Multiplexer`].
#[derive(Debug, Clone, Default)]
pub struct ZellijAdapter {
    /// Optional override for `--config-dir` used by tests. Doesn't
    /// isolate the session namespace (zellij stores that per-UID in
    /// `$XDG_RUNTIME_DIR`) but keeps config-driven behavior
    /// reproducible.
    config_dir: Option<PathBuf>,
}

impl ZellijAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: Some(dir.into()),
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new("zellij");
        if let Some(d) = &self.config_dir {
            c.arg("--config-dir").arg(d);
        }
        c
    }

    /// Detect whether we're currently inside a zellij client. zellij
    /// sets `ZELLIJ` and `ZELLIJ_SESSION_NAME` on every child process.
    pub fn is_inside_zellij() -> bool {
        std::env::var_os("ZELLIJ").is_some() || std::env::var_os("ZELLIJ_SESSION_NAME").is_some()
    }

    /// Create a detached session without attaching. Used by tests.
    /// Blank session — no layout, no command. Useful for exercising
    /// `list_sessions`, `has_session`, and `kill` without spinning
    /// up the full launch path.
    ///
    /// zellij's session registration is briefly async after the CLI
    /// returns — the child process can exit successfully before
    /// `list-sessions` reports the new name. This method polls
    /// `has_session` up to one second so the return is
    /// synchronous-to-visibility, which keeps tests deterministic on
    /// slow CI runners without every test having to retry.
    pub fn create_background(&self, name: &str) -> Result<()> {
        let status = self
            .cmd()
            .arg("attach")
            .arg(name)
            .arg("--create-background")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| friendly_io_err("spawning zellij attach --create-background", e))?;
        if !status.success() {
            bail!("zellij attach --create-background failed for session {name:?}");
        }

        // Wait for the session to appear in list-sessions. 20 × 50ms
        // = 1s max; most runs return on the first check.
        for _ in 0..20 {
            if self.has_session(name)? {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        bail!("zellij session {name:?} was created but did not appear in list within 1s")
    }

    /// Best-effort "are other clients attached to this session?"
    /// check via `zellij action list-clients`. Returns true only when
    /// we can confirm at least one client is connected — unknowns are
    /// treated as false because we don't want to spam a warning in
    /// the common case where list-clients isn't useful for us.
    pub fn other_clients_attached(&self, _name: &str) -> bool {
        // `zellij action list-clients` lists clients on the session
        // the invoker is inside, not an arbitrary named session, so
        // from outside zellij there isn't a reliable CLI probe. We
        // hedge: if we're inside zellij AND can list clients, report
        // what we see; otherwise pessimistically return false.
        if !Self::is_inside_zellij() {
            return false;
        }
        let out = self.cmd().arg("action").arg("list-clients").output();
        match out {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                // One client per line (approximately). If more than
                // one line of real content, there are other clients.
                stdout.lines().filter(|l| !l.trim().is_empty()).count() > 1
            }
            _ => false,
        }
    }

    fn warn_if_other_clients(&self, name: &str) {
        if self.other_clients_attached(name) {
            eprintln!(
                "  warning: other clients may be attached to zellij session {name:?}. \
                zellij has no CLI to force-detach them; if you see screen-size weirdness \
                after attaching, detach the other device manually (Ctrl+Q on the other \
                end) and re-attach here."
            );
        }
    }

    /// Kill + delete the named session. Each step is best-effort so a
    /// half-broken state (killed but not deleted) still gets cleaned
    /// up. Returns Ok whether or not the session existed.
    pub fn kill_and_delete(&self, name: &str) -> Result<()> {
        let _ = self
            .cmd()
            .arg("kill-session")
            .arg(name)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self
            .cmd()
            .arg("delete-session")
            .arg("-f")
            .arg(name)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        Ok(())
    }
}

fn friendly_io_err(context: &str, err: io::Error) -> anyhow::Error {
    if err.kind() == io::ErrorKind::NotFound {
        anyhow!(
            "{context}: zellij binary not found. Install zellij (https://zellij.dev/) and make sure it's on PATH."
        )
    } else {
        anyhow::Error::new(err).context(context.to_string())
    }
}

fn ensure_cwd_exists(cwd: &Path) -> Result<()> {
    if !cwd.exists() {
        return Err(anyhow!("session cwd does not exist: {}", cwd.display()));
    }
    Ok(())
}

fn is_no_session_error(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("no active zellij sessions") || s.contains("no sessions")
}

/// Escape a raw string for embedding inside a KDL double-quoted
/// literal. KDL accepts backslash escapes the same way JSON does for
/// `\"` and `\\`; we don't need to worry about control chars because
/// cwd and command strings don't contain them in practice.
fn escape_kdl(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            _ => out.push(c),
        }
    }
    out
}

/// Generate the KDL layout we hand to `zellij --new-session-with-layout`
/// when starting a session with a specific cwd + command. Wraps our
/// command pane in the stock `default_tab_template` so zellij's
/// familiar tab-bar (top) and status-bar (bottom, with hotkey hints)
/// show up — otherwise a bare `layout { pane {} }` strips them, which
/// looks very broken to anyone who's used zellij before.
///
/// For trivially-a-shell commands (`bash`, `sh`, `zsh`, `$SHELL`) we
/// don't wrap in `-c` — running `bash -c "bash"` sometimes yields a
/// short-lived non-interactive inner shell depending on flags. Running
/// the shell binary directly gets the user's normal interactive shell.
///
/// When the session has env vars set, we route through `env(1)` —
/// each `KEY=VAL` is a separate KDL string arg, which avoids shell-
/// escape gymnastics inside the bash -c payload.
fn render_layout(session: &Session) -> String {
    // Normalize cwd: strip trailing `.` component the walk-up loader
    // leaves behind for `cwd = "."` in TOML. Zellij accepts it but it
    // looks ugly in the status bar and is harmless to trim.
    let cwd_path = {
        let s = session.cwd.display().to_string();
        if let Some(stripped) = s.strip_suffix("/.") {
            stripped.to_string()
        } else if s == "." {
            ".".to_string()
        } else {
            s
        }
    };
    let cwd = escape_kdl(&cwd_path);
    let cmd = escape_kdl(&session.command);

    // Stock zellij default — needed so status-bar + tab-bar plugins
    // render. Values match the out-of-the-box tab template.
    let tab_template = "    default_tab_template {\n        pane size=1 borderless=true {\n            plugin location=\"zellij:tab-bar\"\n        }\n        children\n        pane size=2 borderless=true {\n            plugin location=\"zellij:status-bar\"\n        }\n    }\n";

    // `close_on_exit true`: without it, zellij holds the pane open
    // after the command finishes, showing "EXIT CODE: 0 — press
    // ENTER to close" which confuses users who expect their shell
    // exit to close the session.
    let pane = if session.env.is_empty() {
        if is_shell_command(&session.command) {
            // Run the shell binary directly; no `-c` wrapper.
            format!(
                "    pane cwd=\"{cwd}\" close_on_exit=true {{\n        command \"{cmd}\"\n    }}\n"
            )
        } else {
            format!("    pane cwd=\"{cwd}\" close_on_exit=true {{\n        command \"bash\"\n        args \"-c\" \"{cmd}\"\n    }}\n")
        }
    } else {
        let mut env_args = String::new();
        for (k, v) in &session.env {
            let pair = format!("{k}={v}");
            env_args.push_str(&format!(" \"{}\"", escape_kdl(&pair)));
        }
        if is_shell_command(&session.command) {
            format!("    pane cwd=\"{cwd}\" close_on_exit=true {{\n        command \"env\"\n        args{env_args} \"{cmd}\"\n    }}\n")
        } else {
            format!("    pane cwd=\"{cwd}\" close_on_exit=true {{\n        command \"env\"\n        args{env_args} \"bash\" \"-c\" \"{cmd}\"\n    }}\n")
        }
    };

    format!("layout {{\n{tab_template}{pane}}}\n")
}

/// Is this command "just run a login shell"? Matches the bare shell
/// binary names we expect users to type in a scaffolded workspace.
fn is_shell_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    matches!(trimmed, "bash" | "sh" | "zsh" | "fish" | "ash" | "dash")
}

/// Write the layout to a deterministic path under `$TMPDIR`. One file
/// per session name, overwritten on subsequent launches.
fn write_layout_file(session: &Session, sanitized_name: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("pa-zellij-{sanitized_name}.kdl"));
    fs::write(&path, render_layout(session))
        .map_err(|e| anyhow!("writing zellij layout to {}: {e}", path.display()))?;
    Ok(path)
}

impl Multiplexer for ZellijAdapter {
    fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let out = self
            .cmd()
            .arg("list-sessions")
            .arg("-n")
            .arg("-s")
            .output()
            .map_err(|e| friendly_io_err("spawning zellij list-sessions", e))?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if is_no_session_error(&stderr) {
                return Ok(Vec::new());
            }
            bail!("zellij list-sessions failed: {}", stderr.trim());
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        Ok(stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|name| SessionInfo {
                name: name.to_string(),
                cwd: None,
                attached: None,
            })
            .collect())
    }

    fn has_session(&self, name: &str) -> Result<bool> {
        let sessions = self.list_sessions()?;
        Ok(sessions.iter().any(|s| s.name == name))
    }

    fn attach(&self, name: &str, mode: AttachMode) -> Result<()> {
        if Self::is_inside_zellij() {
            bail!(
                "already inside a zellij session; detach first (Ctrl+Q by default) before attaching to {name:?}"
            );
        }
        // zellij has no CLI-level "detach other clients" flag. On
        // Takeover, warn the user so they can manually detach the
        // other device if they hit screen-size issues. Attach either
        // way — same behavior zellij would give you from a plain
        // `zellij attach`.
        if mode == AttachMode::Takeover {
            self.warn_if_other_clients(name);
        }
        let status = self
            .cmd()
            .arg("attach")
            .arg(name)
            .status()
            .map_err(|e| friendly_io_err("spawning zellij attach", e))?;
        if !status.success() {
            bail!("zellij attach failed for session {name:?}");
        }
        Ok(())
    }

    fn create_and_attach(&self, session: &Session, mpx_name: &str, mode: AttachMode) -> Result<()> {
        let name = mpx_name;
        if self.has_session(name)? {
            return self.attach(name, mode);
        }
        if Self::is_inside_zellij() {
            bail!(
                "already inside a zellij session; detach first (Ctrl+Q by default) before launching session {name:?}"
            );
        }
        ensure_cwd_exists(&session.cwd)?;

        // For shell-only sessions, skip the custom layout entirely and
        // use `zellij attach --create` — this gives the user zellij's
        // native default layout (tab-bar + status-bar guaranteed) with
        // a default shell in the requested cwd. Our custom layouts can
        // fight with user zellij configs and drop the status bar; for
        // the common "just gimme a shell" case, get out of the way.
        if is_shell_command(&session.command) && session.env.is_empty() {
            let status = self
                .cmd()
                .current_dir(&session.cwd)
                .arg("attach")
                .arg(name)
                .arg("--create")
                .status()
                .map_err(|e| friendly_io_err("spawning zellij attach --create", e))?;
            if !status.success() {
                bail!("zellij failed to start session {name:?}");
            }
            return Ok(());
        }

        // Non-shell command (or env overrides): we need a layout to
        // inject `command`. Layout includes default_tab_template so
        // the status bar still renders; pane has close_on_exit so the
        // session cleans up naturally when the command finishes.
        let layout = write_layout_file(session, name)?;

        // `--layout` + `--session` is ambiguous in zellij (>=0.40): with
        // --session set, --layout tries to *add tabs to an existing
        // session of that name*, and fails with "Session not found" if
        // it doesn't exist yet. `--new-session-with-layout` (aka -n)
        // unambiguously forces a fresh session — what we want for
        // create-and-attach.
        let status = self
            .cmd()
            .arg("--session")
            .arg(name)
            .arg("--new-session-with-layout")
            .arg(&layout)
            .status()
            .map_err(|e| friendly_io_err("spawning zellij with layout", e))?;
        if !status.success() {
            bail!("zellij failed to start session {name:?}");
        }
        Ok(())
    }

    fn kill(&self, name: &str) -> Result<()> {
        if !self.has_session(name)? {
            return Ok(());
        }
        self.kill_and_delete(name)
    }

    fn detach_current(&self) -> Result<()> {
        bail!("zellij has no CLI detach action; use the multiplexer's keybind (Ctrl+Q by default)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn cmd_without_config_dir_has_no_flag() {
        let a = ZellijAdapter::new();
        let c = a.cmd();
        let args: Vec<&std::ffi::OsStr> = c.get_args().collect();
        assert!(args.is_empty(), "expected no args, got {args:?}");
    }

    #[test]
    fn cmd_with_config_dir_injects_flag() {
        let a = ZellijAdapter::with_config_dir("/tmp/pa-zj-cfg");
        let args: Vec<String> = a
            .cmd()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec!["--config-dir".to_string(), "/tmp/pa-zj-cfg".to_string()]
        );
    }

    #[test]
    fn no_session_error_matches_known_messages() {
        assert!(is_no_session_error("No active zellij sessions found."));
        assert!(is_no_session_error("No sessions are running currently."));
        assert!(!is_no_session_error("some unrelated error"));
    }

    #[test]
    fn escape_kdl_handles_backslashes_and_quotes() {
        assert_eq!(escape_kdl("plain"), "plain");
        assert_eq!(escape_kdl(r#"has"quote"#), r#"has\"quote"#);
        assert_eq!(escape_kdl(r"has\backslash"), r"has\\backslash");
        assert_eq!(escape_kdl(r#"both\and"quote"#), r#"both\\and\"quote"#);
    }

    #[test]
    fn render_layout_embeds_cwd_and_command() {
        let s = Session {
            name: "x".into(),
            cwd: PathBuf::from("/home/u/code"),
            command: "claude --resume".into(),
            kind: None,
            env: std::collections::BTreeMap::new(),
        };
        let layout = render_layout(&s);
        assert!(layout.contains(r#"cwd="/home/u/code""#));
        assert!(layout.contains(r#"command "bash""#));
        assert!(layout.contains(r#"args "-c" "claude --resume""#));
    }

    #[test]
    fn render_layout_escapes_quotes_in_command() {
        let s = Session {
            name: "x".into(),
            cwd: PathBuf::from("/tmp"),
            command: r#"echo "hi""#.into(),
            kind: None,
            env: std::collections::BTreeMap::new(),
        };
        let layout = render_layout(&s);
        assert!(
            layout.contains(r#"args "-c" "echo \"hi\"""#),
            "bad escape in layout:\n{layout}"
        );
    }

    #[test]
    fn render_layout_routes_through_env_when_env_present() {
        use std::collections::BTreeMap;
        let mut env = BTreeMap::new();
        env.insert("A".into(), "1".into());
        env.insert("B".into(), "two words".into());
        let s = Session {
            name: "x".into(),
            cwd: PathBuf::from("/tmp"),
            command: "claude".into(),
            kind: None,
            env,
        };
        let layout = render_layout(&s);
        assert!(
            layout.contains(r#"command "env""#),
            "missing env command:\n{layout}"
        );
        // BTreeMap → alphabetical args.
        assert!(layout.contains(r#""A=1""#), "missing A=1:\n{layout}");
        assert!(
            layout.contains(r#""B=two words""#),
            "missing B with spaces:\n{layout}"
        );
        assert!(
            layout.contains(r#""bash" "-c" "claude""#),
            "missing bash tail:\n{layout}"
        );
    }

    #[test]
    fn render_layout_includes_default_tab_template_for_status_bar() {
        let s = Session {
            name: "x".into(),
            cwd: PathBuf::from("/tmp"),
            command: "claude".into(),
            kind: None,
            env: std::collections::BTreeMap::new(),
        };
        let layout = render_layout(&s);
        assert!(
            layout.contains("default_tab_template"),
            "missing default_tab_template (status bar will not show):\n{layout}"
        );
        assert!(
            layout.contains("zellij:status-bar"),
            "missing status-bar plugin:\n{layout}"
        );
        assert!(
            layout.contains("zellij:tab-bar"),
            "missing tab-bar plugin:\n{layout}"
        );
    }

    #[test]
    fn render_layout_runs_bare_shell_without_dash_c_wrapper() {
        let s = Session {
            name: "x".into(),
            cwd: PathBuf::from("/tmp"),
            command: "bash".into(),
            kind: None,
            env: std::collections::BTreeMap::new(),
        };
        let layout = render_layout(&s);
        assert!(
            layout.contains(r#"command "bash""#),
            "should run bash directly:\n{layout}"
        );
        assert!(
            !layout.contains(r#"args "-c""#),
            "should not wrap bare shell in -c:\n{layout}"
        );
    }

    #[test]
    fn render_layout_strips_trailing_dot_from_cwd() {
        let s = Session {
            name: "x".into(),
            cwd: PathBuf::from("/home/u/project/."),
            command: "bash".into(),
            kind: None,
            env: std::collections::BTreeMap::new(),
        };
        let layout = render_layout(&s);
        assert!(
            layout.contains(r#"cwd="/home/u/project""#),
            "trailing /. should be stripped:\n{layout}"
        );
    }

    #[test]
    fn render_layout_without_env_is_plain_bash() {
        let s = Session {
            name: "x".into(),
            cwd: PathBuf::from("/tmp"),
            command: "claude".into(),
            kind: None,
            env: std::collections::BTreeMap::new(),
        };
        let layout = render_layout(&s);
        assert!(
            layout.contains(r#"command "bash""#),
            "should use bash directly:\n{layout}"
        );
        assert!(
            !layout.contains(r#"command "env""#),
            "should not route through env:\n{layout}"
        );
    }
}
