//! Multiplexer adapters. See `DESIGN.md` §5.
//!
//! v1 ships only a tmux adapter (next commit); zellij and WezTerm
//! follow in v1.x. The [`Multiplexer`] trait is object-safe so
//! adapters are stored as `Box<dyn Multiplexer>` and new backends slot
//! in without refactoring consumers.

pub mod sanitize;
pub mod session_info;
pub mod tmux;

pub use sanitize::sanitize_session_name;
pub use session_info::SessionInfo;
pub use tmux::TmuxAdapter;

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
    /// blocks until the user detaches from the mpx.
    fn attach(&self, name: &str) -> Result<()>;

    /// Create a session from `session` (name already sanitized by the
    /// caller, cwd absolute, command raw) and attach. Equivalent to
    /// the DESIGN §5 "attach-or-create" shell idiom done imperatively.
    fn create_and_attach(&self, session: &Session) -> Result<()>;

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
                cwd: PathBuf::from("/tmp"),
                attached: false,
            }])
        });

        let got = mock.list_sessions().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "one");
    }
}
