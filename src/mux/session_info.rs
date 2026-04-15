//! What a multiplexer returns from `list_sessions`. Tracked and
//! untracked sessions alike flow through this type — the adapter
//! doesn't know which of its live sessions portagenty spawned.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// Sanitized name as the multiplexer knows it (post-sanitize).
    pub name: String,
    /// The cwd the session was created in, as reported by the mpx.
    pub cwd: PathBuf,
    /// Whether a client is currently attached.
    pub attached: bool,
}
