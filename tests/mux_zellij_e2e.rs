//! End-to-end tests against a real zellij install. Gated behind the
//! `zellij-e2e` cargo feature. See `tests/mux_tmux_e2e.rs` for the
//! tmux-side counterpart.
//!
//! Every test targets a private temporary `$XDG_RUNTIME_DIR`, so parallel
//! nextest processes do not share a Zellij server, registry, or user session
//! namespace. `create_and_attach` and `attach` remain outside this suite because
//! both intentionally own a TTY until detach or exit.

#![cfg(feature = "zellij-e2e")]

use std::time::{SystemTime, UNIX_EPOCH};

use portagenty::mux::{Multiplexer, SessionInfo, ZellijAdapter};

fn unique_name(suffix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("pa-e2e-{}-{nanos}-{suffix}", std::process::id())
}

struct E2e {
    adapter: ZellijAdapter,
    runtime: tempfile::TempDir,
    cleanup_names: Vec<String>,
}

impl E2e {
    fn new() -> Self {
        // Zellij embeds both the runtime and session name in a Unix socket path,
        // whose platform limit is only 107 bytes. Keep the disposable runtime
        // short even when the caller's TMPDIR is a deep agent scratch path.
        #[cfg(unix)]
        let runtime = tempfile::Builder::new()
            .prefix("pa-zj-")
            .tempdir_in("/tmp")
            .expect("create private zellij runtime");
        #[cfg(not(unix))]
        let runtime = tempfile::tempdir().expect("create private zellij runtime");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
                .expect("secure private zellij runtime");
        }
        let adapter = ZellijAdapter::with_runtime_dir(runtime.path());
        Self {
            adapter,
            runtime,
            cleanup_names: Vec::new(),
        }
    }

    fn context(&self, name: &str) -> String {
        format!(
            "session {name:?} in runtime {}",
            self.runtime.path().display()
        )
    }

    fn create_background(&mut self, name: &str) {
        self.cleanup_names.push(name.to_string());
        self.adapter
            .create_background(name)
            .unwrap_or_else(|error| panic!("create {}: {error:#}", self.context(name)));
    }

    fn list_sessions(&self) -> Vec<SessionInfo> {
        self.adapter.list_sessions().unwrap_or_else(|error| {
            panic!(
                "list sessions in runtime {}: {error:#}",
                self.runtime.path().display()
            )
        })
    }
}

impl Drop for E2e {
    fn drop(&mut self) {
        for name in &self.cleanup_names {
            if let Err(error) = self.adapter.kill_and_delete(name) {
                eprintln!("cleanup {} failed: {error:#}", self.context(name));
            }
        }
    }
}

#[test]
fn list_sessions_on_private_runtime_is_empty() {
    let harness = E2e::new();
    let list = harness.list_sessions();
    assert!(
        list.is_empty(),
        "unexpected sessions in private runtime {}: {list:?}",
        harness.runtime.path().display()
    );
}

#[test]
fn create_background_then_list_shows_session() {
    let mut harness = E2e::new();
    let name = unique_name("list-shows");
    harness.create_background(&name);

    let list = harness.list_sessions();
    assert!(
        list.iter().any(|session| session.name == name),
        "expected {} in {list:?}",
        harness.context(&name)
    );
}

#[test]
fn has_session_returns_true_after_create_background() {
    let mut harness = E2e::new();
    let name = unique_name("has");
    assert!(
        !harness.adapter.has_session(&name).unwrap(),
        "{} should not exist yet",
        harness.context(&name)
    );

    harness.create_background(&name);
    assert!(
        harness.adapter.has_session(&name).unwrap(),
        "expected has_session -> true for {}",
        harness.context(&name)
    );
}

#[test]
fn kill_removes_session() {
    let mut harness = E2e::new();
    let name = unique_name("kill");
    harness.create_background(&name);
    assert!(harness.adapter.has_session(&name).unwrap());

    harness.adapter.kill(&name).unwrap();
    assert!(
        !harness.adapter.has_session(&name).unwrap(),
        "{} survived kill",
        harness.context(&name)
    );
}

#[test]
fn kill_is_idempotent_on_missing_session() {
    let harness = E2e::new();
    let name = unique_name("idem");
    harness.adapter.kill(&name).unwrap();
    harness.adapter.kill(&name).unwrap();
}

#[test]
fn session_info_cwd_and_attached_are_none_from_list() {
    let mut harness = E2e::new();
    let name = unique_name("opt");
    harness.create_background(&name);

    let list = harness.list_sessions();
    let found = list
        .iter()
        .find(|session| session.name == name)
        .unwrap_or_else(|| panic!("{} missing from {list:?}", harness.context(&name)));
    assert_eq!(found.cwd, None);
    assert_eq!(found.attached, None);
}

#[test]
fn detach_current_is_not_supported() {
    let harness = E2e::new();
    let error = harness.adapter.detach_current().unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("no CLI detach"),
        "expected 'no CLI detach' hint, got: {message}"
    );
}
