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

pub use sanitize::{sanitize_session_name, workspace_session_name};
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

/// Whether a create-and-attach call created a new multiplexer target or
/// attached to one that already existed. Ownership-aware launchers must not
/// infer this distinction from a successful return value alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreationDisposition {
    Created,
    Existing,
}

/// A multiplexer client process that ran and returned unsuccessfully.
///
/// This is deliberately distinct from an outer [`anyhow::Error`]: an error
/// means validation, preparation, or process spawning failed before a client
/// return could be observed. A `ClientExit` means the client did run, so callers
/// can still print the human workspace/session identity before reporting the
/// abnormal completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// Completion of a blocking multiplexer client command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientCompletion<T> {
    Returned(T),
    Abnormal(ClientExit),
}

impl<T> ClientCompletion<T> {
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> ClientCompletion<U> {
        match self {
            Self::Returned(value) => ClientCompletion::Returned(f(value)),
            Self::Abnormal(exit) => ClientCompletion::Abnormal(exit),
        }
    }

    pub(crate) fn from_status(status: std::process::ExitStatus, value: T) -> Self {
        if status.success() {
            return Self::Returned(value);
        }
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Self::Abnormal(ClientExit {
            code: status.code(),
            signal,
        })
    }
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
    fn attach(&self, name: &str, mode: AttachMode) -> Result<ClientCompletion<()>>;

    /// Create a session from `session` and attach. `mpx_name` is the
    /// workspace-scoped name the mpx should use (e.g. "myproject-shell").
    /// `mode` applies to the attach step. An abnormal client return does not
    /// claim whether creation completed successfully.
    fn create_and_attach(
        &self,
        session: &Session,
        mpx_name: &str,
        mode: AttachMode,
    ) -> Result<ClientCompletion<CreationDisposition>>;

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
                attached: Some(0),
            }])
        });

        let got = mock.list_sessions().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "one");
    }

    #[cfg(unix)]
    #[test]
    fn client_completion_distinguishes_success_code_and_signal() {
        use std::os::unix::process::ExitStatusExt;

        assert_eq!(
            ClientCompletion::from_status(std::process::ExitStatus::from_raw(0), "ok"),
            ClientCompletion::Returned("ok")
        );
        assert_eq!(
            ClientCompletion::from_status(std::process::ExitStatus::from_raw(7 << 8), "unused"),
            ClientCompletion::Abnormal(ClientExit {
                code: Some(7),
                signal: None,
            })
        );
        assert_eq!(
            ClientCompletion::from_status(std::process::ExitStatus::from_raw(15), "unused"),
            ClientCompletion::Abnormal(ClientExit {
                code: None,
                signal: Some(15),
            })
        );
    }
}
