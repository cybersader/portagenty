//! Test-only environment sandbox.
//!
//! Rust runs a crate's unit tests as threads in ONE process, so
//! `std::env::set_var` is process-global: a test that points `HOME` at a
//! scratch path changes `HOME` for every test running concurrently, and for
//! every test that runs afterwards if it never restores the value.
//!
//! That is not hypothetical here. Tests in `state`, `snippets`, and
//! `config::merge` used to set `HOME=/home/test` and leave it set. Depending
//! on scheduling, later tests either wrote into a path that doesn't exist
//! (`Permission denied` from the onboarding wizard) or — worse, when the
//! leaked value happened to be a real directory — into the developer's actual
//! `~/.config/portagenty`. The failure count changed run to run, which is the
//! signature of exactly this problem.
//!
//! [`EnvSandbox`] fixes both halves:
//!
//! * **Restores on drop.** The previous value is captured up front and put
//!   back when the guard falls out of scope, including the "was unset" case.
//!   Restoration happens on panic too, so a failing assertion cannot poison
//!   the rest of the run.
//! * **Serializes.** Overlapping sandboxes would still interleave their
//!   writes, so every guard holds one process-wide lock for its lifetime.
//!   The lock is poison-tolerant: a panicking test releases it in a usable
//!   state rather than cascading `PoisonError` into unrelated tests.
//!
//! Prefer pointing `HOME`/`XDG_*` at a real temporary directory over a
//! synthetic string like `/home/test` — code under test is allowed to create
//! files there, and a nonexistent home makes failures look like bugs in the
//! code rather than in the fixture.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Process-wide lock serializing all environment mutation in tests.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// RAII guard: sets environment variables for its lifetime and restores the
/// previous values (or unsets them) on drop.
pub(crate) struct EnvSandbox {
    // Held for the guard's lifetime so two sandboxes never overlap. Declared
    // before `saved` only for clarity; drop order within the struct doesn't
    // matter because restoration happens explicitly in `drop`.
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(OsString, Option<OsString>)>,
}

impl EnvSandbox {
    /// Acquire the lock without changing anything yet. Use [`Self::set`] to
    /// apply values.
    pub(crate) fn new() -> Self {
        // A panicking test poisons the mutex; the data is `()`, so there is
        // nothing to be corrupted and recovering keeps one failure from
        // cascading into every later env test.
        let lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        Self {
            _lock: lock,
            saved: Vec::new(),
        }
    }

    /// Set `key` to `value`, remembering the prior value for restoration.
    pub(crate) fn set(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        let key = key.as_ref().to_os_string();
        // Only record the ORIGINAL value: re-setting the same key twice must
        // not overwrite the restore target with our own interim value.
        if !self.saved.iter().any(|(k, _)| k == &key) {
            self.saved.push((key.clone(), std::env::var_os(&key)));
        }
        std::env::set_var(&key, value);
        self
    }

    /// Remove `key`, remembering the prior value for restoration.
    pub(crate) fn unset(mut self, key: impl AsRef<OsStr>) -> Self {
        let key = key.as_ref().to_os_string();
        if !self.saved.iter().any(|(k, _)| k == &key) {
            self.saved.push((key.clone(), std::env::var_os(&key)));
        }
        std::env::remove_var(&key);
        self
    }
}

impl Drop for EnvSandbox {
    fn drop(&mut self) {
        for (key, previous) in self.saved.drain(..) {
            match previous {
                Some(value) => std::env::set_var(&key, value),
                None => std::env::remove_var(&key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_previous_value_on_drop() {
        let key = "PA_TEST_ENV_SANDBOX_RESTORE";
        std::env::set_var(key, "original");
        {
            let _sandbox = EnvSandbox::new().set(key, "temporary");
            assert_eq!(std::env::var(key).unwrap(), "temporary");
        }
        assert_eq!(std::env::var(key).unwrap(), "original");
        std::env::remove_var(key);
    }

    #[test]
    fn unsets_a_variable_that_was_not_previously_set() {
        let key = "PA_TEST_ENV_SANDBOX_UNSET";
        std::env::remove_var(key);
        {
            let _sandbox = EnvSandbox::new().set(key, "temporary");
            assert_eq!(std::env::var(key).unwrap(), "temporary");
        }
        assert!(std::env::var_os(key).is_none());
    }

    #[test]
    fn repeated_set_still_restores_the_original() {
        let key = "PA_TEST_ENV_SANDBOX_REPEAT";
        std::env::set_var(key, "original");
        {
            let _sandbox = EnvSandbox::new().set(key, "first").set(key, "second");
            assert_eq!(std::env::var(key).unwrap(), "second");
        }
        assert_eq!(std::env::var(key).unwrap(), "original");
        std::env::remove_var(key);
    }

    #[test]
    fn restores_even_when_the_test_body_panics() {
        let key = "PA_TEST_ENV_SANDBOX_PANIC";
        std::env::set_var(key, "original");
        let result = std::panic::catch_unwind(|| {
            let _sandbox = EnvSandbox::new().set(key, "temporary");
            panic!("boom");
        });
        assert!(result.is_err());
        assert_eq!(std::env::var(key).unwrap(), "original");
        std::env::remove_var(key);
    }

    #[test]
    fn unset_restores_a_previously_set_value() {
        let key = "PA_TEST_ENV_SANDBOX_UNSET_RESTORE";
        std::env::set_var(key, "original");
        {
            let _sandbox = EnvSandbox::new().unset(key);
            assert!(std::env::var_os(key).is_none());
        }
        assert_eq!(std::env::var(key).unwrap(), "original");
        std::env::remove_var(key);
    }
}
