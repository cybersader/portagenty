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
use crate::mux::{sanitize_session_name, Multiplexer, SessionInfo};

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
        Ok(())
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

/// Generate the KDL layout we hand to `zellij --layout` when starting
/// a session with a specific cwd + command. Single pane running
/// `bash -c "<command>"` so shell metacharacters in the command work
/// the same as when the user runs them from their own shell.
fn render_layout(session: &Session) -> String {
    let cwd = escape_kdl(&session.cwd.display().to_string());
    let cmd = escape_kdl(&session.command);
    format!(
        "layout {{\n    pane cwd=\"{cwd}\" {{\n        command \"bash\"\n        args \"-c\" \"{cmd}\"\n    }}\n}}\n"
    )
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

    fn attach(&self, name: &str) -> Result<()> {
        if Self::is_inside_zellij() {
            bail!(
                "already inside a zellij session; detach first (Ctrl+Q by default) before attaching to {name:?}"
            );
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

    fn create_and_attach(&self, session: &Session) -> Result<()> {
        let name = sanitize_session_name(&session.name);
        if self.has_session(&name)? {
            return self.attach(&name);
        }
        if Self::is_inside_zellij() {
            bail!(
                "already inside a zellij session; detach first (Ctrl+Q by default) before launching session {name:?}"
            );
        }
        ensure_cwd_exists(&session.cwd)?;
        let layout = write_layout_file(session, &name)?;

        let status = self
            .cmd()
            .arg("--session")
            .arg(&name)
            .arg("--layout")
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
        };
        let layout = render_layout(&s);
        assert!(
            layout.contains(r#"args "-c" "echo \"hi\"""#),
            "bad escape in layout:\n{layout}"
        );
    }
}
