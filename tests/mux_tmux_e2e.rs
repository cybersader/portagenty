//! End-to-end tests against a real tmux server. Gated behind the
//! `tmux-e2e` cargo feature so the default dev loop stays fast.
//!
//! Each test runs on a private tmux server reached over a temp socket,
//! so parallel nextest runs don't collide with each other or the
//! user's own tmux server. A scopeguard kills the server on drop.
//!
//! Requires `tmux` in PATH. CI installs it via apt on Linux / brew on
//! macOS (see `.github/workflows/ci.yml`).

#![cfg(feature = "tmux-e2e")]

use portagenty::domain::Session;
use portagenty::mux::{Multiplexer, TmuxAdapter};

struct E2e {
    adapter: TmuxAdapter,
    _tmp: assert_fs::TempDir,
    _sock_dir: assert_fs::TempDir,
}

impl E2e {
    fn new() -> Self {
        let sock_dir = assert_fs::TempDir::new().unwrap();
        let socket = sock_dir.path().join("pa.sock");
        let tmp = assert_fs::TempDir::new().unwrap();
        let adapter = TmuxAdapter::with_socket(socket);
        E2e {
            adapter,
            _tmp: tmp,
            _sock_dir: sock_dir,
        }
    }

    fn workdir(&self) -> std::path::PathBuf {
        self._tmp.path().to_path_buf()
    }
}

impl Drop for E2e {
    fn drop(&mut self) {
        let _ = self.adapter.kill_server();
    }
}

#[test]
fn list_sessions_on_empty_server_returns_empty() {
    let h = E2e::new();
    let list = h.adapter.list_sessions().expect("list_sessions");
    assert!(list.is_empty(), "unexpected sessions: {list:?}");
}

#[test]
fn has_session_false_on_empty_server() {
    let h = E2e::new();
    assert!(!h.adapter.has_session("anything").unwrap());
}

#[test]
fn create_detached_then_list_and_has_session() {
    let h = E2e::new();

    let sess = Session {
        name: "alpha".into(),
        cwd: h.workdir(),
        command: "sleep 60".into(),
        kind: None,
        env: std::collections::BTreeMap::new(),
    };
    h.adapter.create_detached(&sess).expect("create_detached");

    assert!(h.adapter.has_session("alpha").unwrap());
    let list = h.adapter.list_sessions().expect("list");
    assert!(
        list.iter().any(|s| s.name == "alpha"),
        "expected alpha in {list:?}"
    );
}

#[test]
fn kill_is_idempotent() {
    let h = E2e::new();
    h.adapter
        .kill("nonexistent")
        .expect("kill should not error");

    let sess = Session {
        name: "beta".into(),
        cwd: h.workdir(),
        command: "sleep 60".into(),
        kind: None,
        env: std::collections::BTreeMap::new(),
    };
    h.adapter.create_detached(&sess).unwrap();
    assert!(h.adapter.has_session("beta").unwrap());

    h.adapter.kill("beta").unwrap();
    assert!(!h.adapter.has_session("beta").unwrap());

    // Killing again on a now-empty server is also a no-op.
    h.adapter.kill("beta").unwrap();
}

#[test]
fn sanitization_round_trip_through_create_detached() {
    let h = E2e::new();

    // Feed a name that forces sanitization; the adapter records it
    // under the sanitized form and list_sessions reports it as such.
    let sess = Session {
        name: "has spaces:and:colons".into(),
        cwd: h.workdir(),
        command: "sleep 60".into(),
        kind: None,
        env: std::collections::BTreeMap::new(),
    };
    h.adapter.create_detached(&sess).unwrap();

    let list = h.adapter.list_sessions().unwrap();
    let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"has_spaces_and_colons"),
        "sanitized name missing: {names:?}"
    );
}

#[test]
fn create_detached_reports_cwd() {
    let h = E2e::new();
    let cwd = h.workdir();
    let sess = Session {
        name: "gamma".into(),
        cwd: cwd.clone(),
        command: "sleep 60".into(),
        kind: None,
        env: std::collections::BTreeMap::new(),
    };
    h.adapter.create_detached(&sess).unwrap();

    let list = h.adapter.list_sessions().unwrap();
    let found = list.iter().find(|s| s.name == "gamma").unwrap();
    // tmux canonicalizes, so don't require exact match — just that
    // the path reported is a real path and matches our tempdir's last
    // component. tmux adapter always populates cwd + attached.
    let found_cwd = found.cwd.as_ref().expect("tmux adapter should set cwd");
    let last = found_cwd
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    let expected = cwd
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    assert_eq!(last, expected);
    assert_eq!(found.attached, Some(false));
}

#[test]
fn create_detached_errors_when_cwd_missing() {
    let h = E2e::new();
    let bad_cwd = h.workdir().join("does-not-exist");
    let sess = Session {
        name: "delta".into(),
        cwd: bad_cwd,
        command: "sleep 60".into(),
        kind: None,
        env: std::collections::BTreeMap::new(),
    };
    let err = h.adapter.create_detached(&sess).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("cwd does not exist"), "unexpected: {msg}");
}
