//! tmux adapter. Shells out to the `tmux` CLI via `std::process::Command`.
//!
//! An optional `socket` path isolates the adapter to a private tmux
//! server (tmux `-S <path>`). Production use leaves this `None` to
//! share the user's default server; tests pass a per-test socket so
//! concurrent nextest runs don't collide.

use anyhow::{anyhow, bail, Result};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::domain::Session;
use crate::mux::{sanitize_session_name, AttachMode, Multiplexer, SessionInfo};

/// Wrap a std::io::Error that fired during a tmux invocation. We
/// lift `NotFound` into a clear "tmux isn't installed or isn't in
/// PATH" message instead of surfacing the raw os error 2.
fn friendly_io_err(context: &str, err: io::Error) -> anyhow::Error {
    if err.kind() == io::ErrorKind::NotFound {
        anyhow!(
            "{context}: tmux binary not found. Install tmux (`sudo apt install tmux` on Debian/Ubuntu, `brew install tmux` on macOS, or your distro's package manager) or `cargo install` a `tmux-bin` equivalent and make sure it's on PATH."
        )
    } else {
        anyhow::Error::new(err).context(context.to_string())
    }
}

/// tmux-backed [`Multiplexer`].
///
/// A single instance is cheap to clone; the only state is the optional
/// socket path.
#[derive(Debug, Clone, Default)]
pub struct TmuxAdapter {
    socket: Option<PathBuf>,
}

impl TmuxAdapter {
    /// Default server (shared with the user's other tmux sessions).
    pub fn new() -> Self {
        Self::default()
    }

    /// Private tmux server at the given socket path. Used in tests
    /// for isolation; not typically what end users want.
    pub fn with_socket(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: Some(socket.into()),
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new("tmux");
        if let Some(s) = &self.socket {
            c.arg("-S").arg(s);
        }
        c
    }

    /// Create a detached session without attaching. Distinct from
    /// [`Multiplexer::create_and_attach`] so tests can drive the
    /// create path without taking over the controlling TTY.
    pub fn create_detached(&self, session: &Session) -> Result<()> {
        let name = sanitize_session_name(&session.name);
        if self.has_session(&name)? {
            return Ok(());
        }
        ensure_cwd_exists(&session.cwd)?;
        let status = self
            .cmd()
            .arg("new-session")
            .arg("-d")
            .arg("-s")
            .arg(&name)
            .arg("-c")
            .arg(&session.cwd)
            .arg(&session.command)
            .status()
            .map_err(|e| friendly_io_err("spawning tmux new-session", e))?;
        if !status.success() {
            bail!("tmux new-session failed for session {name:?}");
        }
        Ok(())
    }

    /// Stop the tmux server this adapter is pointed at. Used in
    /// tests to tear down the isolated server cleanly.
    pub fn kill_server(&self) -> Result<()> {
        let _ = self
            .cmd()
            .arg("kill-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        Ok(())
    }
}

fn ensure_cwd_exists(cwd: &Path) -> Result<()> {
    if !cwd.exists() {
        return Err(anyhow!("session cwd does not exist: {}", cwd.display()));
    }
    Ok(())
}

fn is_no_server_error(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("no server running") || s.contains("no sessions") || s.contains("error connecting")
}

impl Multiplexer for TmuxAdapter {
    fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let out = self
            .cmd()
            .arg("list-sessions")
            .arg("-F")
            .arg("#{session_name}|#{session_path}|#{session_attached}")
            .output()
            .map_err(|e| friendly_io_err("spawning tmux list-sessions", e))?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if is_no_server_error(&stderr) {
                return Ok(Vec::new());
            }
            bail!("tmux list-sessions failed: {}", stderr.trim());
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut sessions = Vec::new();
        for line in stdout.lines().filter(|l| !l.is_empty()) {
            let mut parts = line.splitn(3, '|');
            let name = parts
                .next()
                .ok_or_else(|| anyhow!("tmux list-sessions: missing name in line {line:?}"))?
                .to_string();
            let cwd = parts
                .next()
                .ok_or_else(|| anyhow!("tmux list-sessions: missing path in line {line:?}"))?;
            let attached = parts.next().ok_or_else(|| {
                anyhow!("tmux list-sessions: missing attached flag in line {line:?}")
            })?;
            sessions.push(SessionInfo {
                name,
                cwd: Some(PathBuf::from(cwd)),
                attached: Some(attached == "1"),
            });
        }
        Ok(sessions)
    }

    fn has_session(&self, name: &str) -> Result<bool> {
        let status = self
            .cmd()
            .arg("has-session")
            .arg("-t")
            .arg(name)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| friendly_io_err("spawning tmux has-session", e))?;
        Ok(status.success())
    }

    fn attach(&self, name: &str, mode: AttachMode) -> Result<()> {
        let mut cmd = self.cmd();
        cmd.arg("attach-session").arg("-t").arg(name);
        if mode == AttachMode::Takeover {
            // tmux's `-d` detaches other clients on attach. Session
            // keeps running; only the other *clients* get bumped.
            cmd.arg("-d");
        }
        let status = cmd
            .status()
            .map_err(|e| friendly_io_err("spawning tmux attach-session", e))?;
        if !status.success() {
            bail!("tmux attach-session failed for {name:?}");
        }
        Ok(())
    }

    fn create_and_attach(&self, session: &Session, mode: AttachMode) -> Result<()> {
        self.create_detached(session)?;
        let name = sanitize_session_name(&session.name);
        self.attach(&name, mode)
    }

    fn kill(&self, name: &str) -> Result<()> {
        if !self.has_session(name)? {
            return Ok(());
        }
        let status = self
            .cmd()
            .arg("kill-session")
            .arg("-t")
            .arg(name)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| friendly_io_err("spawning tmux kill-session", e))?;
        if !status.success() {
            bail!("tmux kill-session failed for {name:?}");
        }
        Ok(())
    }

    fn detach_current(&self) -> Result<()> {
        let status = self
            .cmd()
            .arg("detach-client")
            .status()
            .map_err(|e| friendly_io_err("spawning tmux detach-client", e))?;
        if !status.success() {
            bail!("tmux detach-client failed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_without_socket_has_no_s_flag() {
        let a = TmuxAdapter::new();
        let c = a.cmd();
        // The program is tmux with no args; `-S` appears only when
        // a socket is set.
        let args: Vec<&std::ffi::OsStr> = c.get_args().collect();
        assert!(args.is_empty(), "expected no args, got {args:?}");
    }

    #[test]
    fn cmd_with_socket_injects_s_flag() {
        let a = TmuxAdapter::with_socket("/tmp/pa.sock");
        let c = a.cmd();
        let args: Vec<String> = c
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["-S".to_string(), "/tmp/pa.sock".to_string()]);
    }

    #[test]
    fn is_no_server_error_catches_expected_messages() {
        assert!(is_no_server_error("no server running on /tmp/sock"));
        assert!(is_no_server_error("error connecting to /tmp/sock"));
        assert!(is_no_server_error("no sessions"));
        assert!(!is_no_server_error("some unrelated tmux error"));
    }

    /// Build the args `attach` would pass to the `tmux` binary, without
    /// actually spawning it. Used only by the test below — mirrors the
    /// logic in `impl Multiplexer for TmuxAdapter::attach`.
    fn attach_args_for(name: &str, mode: AttachMode) -> Vec<String> {
        let mut cmd = Command::new("tmux");
        cmd.arg("attach-session").arg("-t").arg(name);
        if mode == AttachMode::Takeover {
            cmd.arg("-d");
        }
        cmd.get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn attach_in_takeover_mode_includes_dash_d() {
        let args = attach_args_for("claude", AttachMode::Takeover);
        assert_eq!(args, vec!["attach-session", "-t", "claude", "-d"]);
    }

    #[test]
    fn attach_in_shared_mode_omits_dash_d() {
        let args = attach_args_for("claude", AttachMode::Shared);
        assert_eq!(args, vec!["attach-session", "-t", "claude"]);
    }

    #[test]
    fn attach_mode_default_is_takeover() {
        assert_eq!(AttachMode::default(), AttachMode::Takeover);
    }
}
