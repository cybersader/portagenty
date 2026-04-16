//! End-to-end tests against a real zellij install. Gated behind the
//! `zellij-e2e` cargo feature. See `tests/mux_tmux_e2e.rs` for the
//! tmux-side counterpart.
//!
//! Caveats specific to zellij:
//!
//! - zellij doesn't support per-socket isolation the way tmux's `-S`
//!   does. Session names are in a shared per-UID namespace. Tests
//!   therefore use a unique prefix (`pa-e2e-<pid>-<nanos>-`) and
//!   filter when reading back from `list_sessions`.
//! - `create_and_attach` and `attach` cannot be exercised here: both
//!   would either block on the TTY (outside zellij) or be rejected
//!   by zellij's nested-session check (inside zellij, where CI
//!   runners effectively aren't but WSL might be). These paths are
//!   covered by their own unit tests; the e2e suite stays in the
//!   create-background / list / has / kill quadrant where it can
//!   actually automate cleanly.

#![cfg(feature = "zellij-e2e")]

use std::time::{SystemTime, UNIX_EPOCH};

use portagenty::mux::{Multiplexer, ZellijAdapter};

fn test_prefix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("pa-e2e-{}-{}-", std::process::id(), nanos)
}

fn unique_name(prefix: &str, suffix: &str) -> String {
    format!("{prefix}{suffix}")
}

/// RAII guard that kills + deletes a session on drop. Test bodies can
/// early-return without worrying about leaks.
struct SessionGuard<'a> {
    adapter: &'a ZellijAdapter,
    names: Vec<String>,
}

impl<'a> SessionGuard<'a> {
    fn new(adapter: &'a ZellijAdapter) -> Self {
        Self {
            adapter,
            names: Vec::new(),
        }
    }

    fn track(&mut self, name: impl Into<String>) {
        self.names.push(name.into());
    }
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        for n in &self.names {
            let _ = self.adapter.kill_and_delete(n);
        }
    }
}

#[test]
fn list_sessions_succeeds_and_does_not_error() {
    let a = ZellijAdapter::new();
    let list = a.list_sessions().expect("list_sessions");
    // Shared namespace: can't assert emptiness. Just make sure no
    // session in the list has a name collision with our prefix.
    let prefix = test_prefix();
    assert!(
        !list.iter().any(|s| s.name.starts_with(&prefix)),
        "stale pa-e2e sessions visible: {list:?}"
    );
}

#[test]
fn create_background_then_list_shows_session() {
    let a = ZellijAdapter::new();
    let mut guard = SessionGuard::new(&a);

    let name = unique_name(&test_prefix(), "list-shows");
    a.create_background(&name).expect("create_background");
    guard.track(&name);

    // Zellij's session registration is asynchronous — poll briefly
    // before giving up so CI runners with slow I/O don't flake.
    let mut found = false;
    for _ in 0..10 {
        let list = a.list_sessions().expect("list");
        if list.iter().any(|s| s.name == name) {
            found = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(found, "expected {name:?} in list after 2s of polling");
}

#[test]
fn has_session_returns_true_after_create_background() {
    let a = ZellijAdapter::new();
    let mut guard = SessionGuard::new(&a);

    let name = unique_name(&test_prefix(), "has");
    assert!(
        !a.has_session(&name).unwrap(),
        "session shouldn't exist yet"
    );
    a.create_background(&name).unwrap();
    guard.track(&name);
    assert!(
        a.has_session(&name).unwrap(),
        "expected has_session -> true"
    );
}

#[test]
fn kill_removes_session() {
    let a = ZellijAdapter::new();

    let name = unique_name(&test_prefix(), "kill");
    a.create_background(&name).unwrap();
    assert!(a.has_session(&name).unwrap());

    a.kill(&name).unwrap();
    assert!(!a.has_session(&name).unwrap());
}

#[test]
fn kill_is_idempotent_on_missing_session() {
    let a = ZellijAdapter::new();
    let name = unique_name(&test_prefix(), "idem");
    // Session doesn't exist. kill should succeed silently.
    a.kill(&name).unwrap();
    a.kill(&name).unwrap();
}

#[test]
fn session_info_cwd_and_attached_are_none_from_list() {
    let a = ZellijAdapter::new();
    let mut guard = SessionGuard::new(&a);

    let name = unique_name(&test_prefix(), "opt");
    a.create_background(&name).unwrap();
    guard.track(&name);

    // Under CI load zellij's session registry propagation can trail
    // create_background's own poll by a beat or two when other e2e
    // tests are racing create/kill on the same $XDG_RUNTIME_DIR.
    // Retry the lookup briefly rather than failing on the first miss —
    // same contract, just with slack matching real observed timing.
    let mut found = None;
    for _ in 0..20 {
        let list = a.list_sessions().unwrap();
        if let Some(info) = list.iter().find(|s| s.name == name) {
            found = Some(info.clone());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let found = found.expect("session should be in list within 1s");
    // zellij doesn't expose these; adapter reports None to match the
    // SessionInfo contract.
    assert_eq!(found.cwd, None);
    assert_eq!(found.attached, None);
}

#[test]
fn detach_current_is_not_supported() {
    let a = ZellijAdapter::new();
    let err = a.detach_current().unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no CLI detach"),
        "expected 'no CLI detach' hint, got: {msg}"
    );
}
