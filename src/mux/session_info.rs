//! What a multiplexer returns from `list_sessions`. Tracked and
//! untracked sessions alike flow through this type — the adapter
//! doesn't know which of its live sessions portagenty spawned.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// Sanitized name as the multiplexer knows it (post-sanitize).
    pub name: String,
    /// The cwd the session was created in, as reported by the mpx.
    /// `None` when the mpx doesn't expose per-session cwd — zellij
    /// falls into this category; tmux always sets it.
    pub cwd: Option<PathBuf>,
    /// Number of clients currently attached to this session.
    /// `Some(0)` = live but unattached; `Some(n >= 1)` = n clients.
    /// `None` when the multiplexer doesn't expose a per-session
    /// count (zellij). Tmux reports the real count.
    pub attached: Option<u32>,
}
