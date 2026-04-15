//! Split state store. See `DESIGN.md` §4.
//!
//! Durable, machine-local, volatile-ish: which workspace+session the
//! user most recently launched. This enables v1.x's Recent view; in v1
//! nothing reads it yet, only `record_launch` writes. Keeping the
//! write side first means the dataset exists by the time the Recent
//! view ships.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_FILENAME: &str = "state.toml";
const MAX_RECENT: usize = 50;

/// Full shape of `$XDG_STATE_HOME/portagenty/state.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct StateFile {
    #[serde(default, rename = "recent")]
    pub recent: Vec<RecentEntry>,
}

/// One launch record. Most-recent entries live at the front of
/// `StateFile::recent`. `launched_at_unix` is seconds since the Unix
/// epoch — simple enough to sort and compare, portable across
/// platforms without pulling in chrono.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct RecentEntry {
    pub workspace_file: PathBuf,
    pub session_name: String,
    pub launched_at_unix: u64,
}

/// Resolved `$XDG_STATE_HOME/portagenty/` (or the platform-appropriate
/// equivalent). The `directories` crate doesn't expose a state-dir
/// helper, so this is a small manual implementation of the XDG state
/// dir spec with a USERPROFILE fallback for Windows.
pub fn state_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("portagenty"));
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| anyhow!("neither HOME nor USERPROFILE is set"))?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("portagenty"))
}

/// Path to the canonical state file.
pub fn state_file_path() -> Result<PathBuf> {
    Ok(state_dir()?.join(STATE_FILENAME))
}

/// Load the state file, or return an empty default if it doesn't
/// exist. A malformed state file *is* an error — better to surface
/// than to silently discard user state.
pub fn load() -> Result<StateFile> {
    load_from(&state_file_path()?)
}

/// Load from an explicit path. Used by tests.
pub fn load_from(path: &Path) -> Result<StateFile> {
    if !path.is_file() {
        return Ok(StateFile::default());
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("reading state file {}", path.display()))?;
    toml::from_str::<StateFile>(&contents)
        .with_context(|| format!("parsing state file {}", path.display()))
}

/// Atomic write: serialize to TOML, write to a sibling `.tmp`,
/// rename onto the canonical name. Crash between write and rename
/// leaves the old state intact; crash after rename is the new state.
pub fn save(state: &StateFile) -> Result<()> {
    save_to(&state_file_path()?, state)
}

/// Save to an explicit path. Used by tests.
pub fn save_to(path: &Path, state: &StateFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let serialized = toml::to_string_pretty(state).context("serializing state")?;
    let tmp = path.with_extension("toml.tmp");
    {
        let mut f =
            fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(serialized.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path).with_context(|| format!("renaming onto {}", path.display()))?;
    Ok(())
}

/// Record that `session_name` was launched in `workspace_file`.
/// Deduplicates by (workspace_file, session_name) so the same
/// session appears only once, always most-recent-first. Clamps the
/// total list at MAX_RECENT entries so the file stays tiny.
pub fn record_launch(workspace_file: &Path, session_name: &str) -> Result<()> {
    record_launch_to(
        &state_file_path()?,
        workspace_file,
        session_name,
        now_unix(),
    )
}

/// Explicit-path variant for tests.
pub fn record_launch_to(
    path: &Path,
    workspace_file: &Path,
    session_name: &str,
    now: u64,
) -> Result<()> {
    let mut state = load_from(path)?;
    state
        .recent
        .retain(|e| !(e.workspace_file == workspace_file && e.session_name == session_name));
    state.recent.insert(
        0,
        RecentEntry {
            workspace_file: workspace_file.to_path_buf(),
            session_name: session_name.to_string(),
            launched_at_unix: now,
        },
    );
    state.recent.truncate(MAX_RECENT);
    save_to(path, &state)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Most-recent launch timestamp (unix seconds) for a given workspace
/// file, across any session. `None` if the workspace has never been
/// launched via `pa`. Used by the picker to sort workspaces by
/// recency.
pub fn last_launch_for_workspace(workspace_file: &Path) -> Option<u64> {
    let state = load().ok()?;
    state
        .recent
        .iter()
        .filter(|e| e.workspace_file == workspace_file)
        .map(|e| e.launched_at_unix)
        .max()
}

/// Most-recent launch timestamp for a specific (workspace, session)
/// pair. `None` if this session has never been launched via `pa`.
/// Used by the session list to render a "2h ago" column on live rows.
pub fn last_launch_for_session(workspace_file: &Path, session_name: &str) -> Option<u64> {
    let state = load().ok()?;
    state
        .recent
        .iter()
        .find(|e| e.workspace_file == workspace_file && e.session_name == session_name)
        .map(|e| e.launched_at_unix)
}

/// Format a unix timestamp as a terse relative-time string suitable
/// for a one-column display ("just now", "5m ago", "2h ago", "3d
/// ago", "2w ago"). Anchors to current wall-clock time. Returns an
/// empty string for `None` input so callers can pad unconditionally.
pub fn relative_time(ts_unix: Option<u64>) -> String {
    let Some(ts) = ts_unix else {
        return String::new();
    };
    let now = now_unix();
    let delta = now.saturating_sub(ts);
    match delta {
        0..=30 => "just now".to_string(),
        31..=3599 => format!("{}m ago", delta / 60),
        3600..=86_399 => format!("{}h ago", delta / 3600),
        86_400..=604_799 => format!("{}d ago", delta / 86_400),
        _ => format!("{}w ago", delta / 604_800),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_uses_xdg_state_home_when_set() {
        std::env::set_var("XDG_STATE_HOME", "/tmp/xdg-state");
        let d = state_dir().unwrap();
        assert_eq!(d, PathBuf::from("/tmp/xdg-state/portagenty"));
    }

    #[test]
    fn state_dir_falls_back_to_home_dot_local_state() {
        std::env::remove_var("XDG_STATE_HOME");
        std::env::set_var("HOME", "/home/test");
        let d = state_dir().unwrap();
        assert_eq!(d, PathBuf::from("/home/test/.local/state/portagenty"));
    }

    #[test]
    fn load_returns_default_when_file_missing() {
        let p = std::path::PathBuf::from("/nonexistent/no-such-state.toml");
        let s = load_from(&p).unwrap();
        assert!(s.recent.is_empty());
    }

    #[test]
    fn save_then_load_round_trip() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("state.toml");

        let state = StateFile {
            recent: vec![RecentEntry {
                workspace_file: PathBuf::from("/ws/foo.portagenty.toml"),
                session_name: "claude".into(),
                launched_at_unix: 1_700_000_000,
            }],
        };
        save_to(&path, &state).unwrap();

        let back = load_from(&path).unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn record_launch_prepends_entry() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("state.toml");

        record_launch_to(&path, Path::new("/a.portagenty.toml"), "s1", 100).unwrap();
        record_launch_to(&path, Path::new("/a.portagenty.toml"), "s2", 200).unwrap();

        let state = load_from(&path).unwrap();
        assert_eq!(state.recent.len(), 2);
        // Most-recent at the front.
        assert_eq!(state.recent[0].session_name, "s2");
        assert_eq!(state.recent[0].launched_at_unix, 200);
        assert_eq!(state.recent[1].session_name, "s1");
    }

    #[test]
    fn record_launch_dedups_by_workspace_plus_session_name() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("state.toml");

        let ws = Path::new("/a.portagenty.toml");
        record_launch_to(&path, ws, "shared", 100).unwrap();
        record_launch_to(&path, ws, "other", 200).unwrap();
        record_launch_to(&path, ws, "shared", 300).unwrap();

        let state = load_from(&path).unwrap();
        assert_eq!(state.recent.len(), 2);
        assert_eq!(state.recent[0].session_name, "shared");
        assert_eq!(state.recent[0].launched_at_unix, 300);
        assert_eq!(state.recent[1].session_name, "other");
    }

    #[test]
    fn record_launch_clamps_at_max_recent() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("state.toml");

        for i in 0..MAX_RECENT + 10 {
            record_launch_to(
                &path,
                Path::new("/a.portagenty.toml"),
                &format!("s{i}"),
                i as u64,
            )
            .unwrap();
        }

        let state = load_from(&path).unwrap();
        assert_eq!(state.recent.len(), MAX_RECENT);
        // Most-recent is s(MAX+9); oldest kept is s10 (0..9 got trimmed).
        assert_eq!(state.recent[0].session_name, format!("s{}", MAX_RECENT + 9));
    }

    #[test]
    fn save_creates_parent_directory_if_missing() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("deep/sub/dir/state.toml");
        save_to(&path, &StateFile::default()).unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn record_launch_survives_malformed_tmp_leftover() {
        // A leftover `state.toml.tmp` from a prior crash shouldn't stop
        // the next write. We fs::rename onto the canonical name, which
        // works whether or not the tmp existed before.
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("state.toml");
        let tmp_file = tmp.path().join("state.toml.tmp");
        fs::write(&tmp_file, "junk").unwrap();

        record_launch_to(&path, Path::new("/a.portagenty.toml"), "s", 1).unwrap();
        assert!(path.is_file());
    }
}
