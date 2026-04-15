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
    /// Whether a client is currently attached. `None` when the mpx
    /// doesn't expose this cheaply; adapters that can tell set it.
    pub attached: Option<bool>,
}
