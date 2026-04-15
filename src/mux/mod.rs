//! Multiplexer adapters. See `DESIGN.md` §5.
//!
//! v1 ships only a tmux adapter (next commit); zellij and WezTerm
//! follow in v1.x. The [`Multiplexer`] trait is object-safe so
//! adapters are stored as `Box<dyn Multiplexer>` and new backends slot
//! in without refactoring consumers.

pub mod sanitize;
pub mod session_info;
pub mod tmux;
pub mod zellij;

pub use sanitize::sanitize_session_name;
pub use session_info::SessionInfo;
pub use tmux::TmuxAdapter;
pub use zellij::ZellijAdapter;

/// How an attach behaves with respect to any clients already connected
/// to the same session.
///
/// Driven by the cross-device use case: SSH in from a phone, run
/// `pa claim` or `pa launch`, the session instantly reshapes to the
/// current terminal's size because the previous (desktop) client
/// gets detached. The session itself keeps running — this is not
/// a kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AttachMode {
    /// Detach any other clients first, then attach. This is
    /// `tmux attach -d` semantics. Session is preserved; only the
    /// other device's *client* is disconnected and can re-attach
    /// later. Fixes the "screen size stuck to whichever client was
    /// smallest" issue inherent to multi-client mpx sessions.
    #[default]
    Takeover,
    /// Attach without touching other clients. Multiple devices can
    /// watch the same session at once. Useful for pair-style
    /// workflows or when you explicitly want read-only shadowing.
    Shared,
}

use anyhow::Result;

use crate::domain::Session;

/// A handle on a concrete multiplexer (tmux, zellij, or WezTerm).
/// Every method takes an already-sanitized name and is expected to
/// sanitize again internally if it ever needs to build a new one —
/// the contract is "portagenty passes the sanitized form and so does
/// the adapter," keeping both sides in sync.
#[cfg_attr(test, mockall::automock)]
pub trait Multiplexer {
    /// All live sessions the mpx can see, including ones portagenty
    /// did not launch. Used to populate the "untracked sessions" pane
    /// in the TUI (v1.x feature) and to decide attach-vs-create.
    fn list_sessions(&self) -> Result<Vec<SessionInfo>>;

    /// Cheap existence check. `name` is the sanitized form.
    fn has_session(&self, name: &str) -> Result<bool>;

    /// Attach the current TTY to an existing session. The process
    /// blocks until the user detaches from the mpx. `mode` controls
    /// whether other clients currently attached to the same session
    /// get bumped or left in place; see [`AttachMode`].
    fn attach(&self, name: &str, mode: AttachMode) -> Result<()>;

    /// Create a session from `session` and attach. `mode` applies to
    /// the attach step — a freshly-created session has no other
    /// clients, so mode only matters when the session already exists.
    fn create_and_attach(&self, session: &Session, mode: AttachMode) -> Result<()>;

    /// Kill a session by sanitized name. No-op when the session does
    /// not exist.
    fn kill(&self, name: &str) -> Result<()>;

    /// Detach any currently-attached client from the mpx. Used by the
    /// TUI's "back to workspace tree" action.
    fn detach_current(&self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Object-safety smoke test: constructs a mock and stores it
    // behind `Box<dyn Multiplexer>`. Compile-time only; no runtime
    // assertions needed.
    #[test]
    fn mock_multiplexer_fits_in_box_dyn() {
        let mock = MockMultiplexer::new();
        let _boxed: Box<dyn Multiplexer> = Box::new(mock);
    }

    #[test]
    fn mock_expectations_drive_has_session() {
        let mut mock = MockMultiplexer::new();
        mock.expect_has_session()
            .withf(|n| n == "claude")
            .times(1)
            .returning(|_| Ok(true));
        mock.expect_has_session()
            .withf(|n| n == "missing")
            .times(1)
            .returning(|_| Ok(false));

        assert!(mock.has_session("claude").unwrap());
        assert!(!mock.has_session("missing").unwrap());
    }

    #[test]
    fn mock_expectations_drive_list_sessions() {
        let mut mock = MockMultiplexer::new();
        mock.expect_list_sessions().returning(|| {
            Ok(vec![SessionInfo {
                name: "one".into(),
                cwd: Some(PathBuf::from("/tmp")),
                attached: Some(false),
            }])
        });

        let got = mock.list_sessions().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "one");
    }
}
