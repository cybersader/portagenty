//! URL protocol handler for `pa://...` links. Lets the OS map clicks
//! on `pa://open/...` (etc.) to actually opening a workspace. See
//! `pa protocol --help` for per-OS registration.
//!
//! Design:
//! - Pure parsing here (platform-agnostic, easy to test).
//! - CLI glue in `cli::open_url` dispatches parsed actions to existing
//!   commands (`tui::run` with a path, `cli::launch`, etc.).
//! - Per-OS registration helpers in `protocol::register` pick a
//!   terminal emulator (auto-detected or user-specified) and write
//!   the appropriate OS-level mapping (.desktop / registry / plist).
//!
//! Grammar (v1):
//!
//!   pa://open/<url-encoded-path>       → open workspace TUI at path
//!   pa://shell/<url-encoded-path>      → drop to plain shell at path
//!   pa://workspace/<uuid>              → open workspace by id (scans registry)
//!   pa://launch/<uuid>/<session-name>  → launch a specific session
//!
//! Unknown actions return a clear error rather than silently falling
//! back — user-facing errors beat confusing clicks.

pub mod register;

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

/// Parsed pa:// URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolAction {
    /// Open the session TUI for the workspace at `path`.
    Open(PathBuf),
    /// Drop into a plain shell at `path`.
    Shell(PathBuf),
    /// Look up a workspace by UUID in the global registry and open it.
    WorkspaceById(String),
    /// Launch a specific session in a workspace (by UUID).
    LaunchSession {
        workspace_id: String,
        session: String,
    },
}

/// Parse a `pa://...` URL into a structured action. Accepts raw URLs
/// as they arrive from the OS click handler (percent-encoded).
pub fn parse(url: &str) -> Result<ProtocolAction> {
    let rest = url
        .strip_prefix("pa://")
        .ok_or_else(|| anyhow!("not a pa:// URL: {url:?}"))?;
    let (action, payload) = split_once(rest);
    match action {
        "open" => {
            let path = decoded_path(payload).context("decoding open path")?;
            Ok(ProtocolAction::Open(path))
        }
        "shell" => {
            let path = decoded_path(payload).context("decoding shell path")?;
            Ok(ProtocolAction::Shell(path))
        }
        "workspace" => {
            let id = trim_trailing_slash(payload);
            if id.is_empty() {
                return Err(anyhow!("pa://workspace/ requires a workspace id"));
            }
            Ok(ProtocolAction::WorkspaceById(id.to_string()))
        }
        "launch" => {
            let (id, session) = split_once(payload);
            let id = trim_trailing_slash(id);
            let session =
                percent_decode(trim_trailing_slash(session)).context("decoding session name")?;
            if id.is_empty() || session.is_empty() {
                return Err(anyhow!(
                    "pa://launch/<workspace-id>/<session-name> requires both"
                ));
            }
            Ok(ProtocolAction::LaunchSession {
                workspace_id: id.to_string(),
                session,
            })
        }
        "" => Err(anyhow!("pa:// URL missing an action (e.g. open / shell)")),
        other => Err(anyhow!(
            "unknown pa:// action {other:?} (supported: open / shell / workspace / launch)"
        )),
    }
}

/// Split on the first `/`. Returns (head, tail), tail may be empty.
fn split_once(s: &str) -> (&str, &str) {
    match s.find('/') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    }
}

fn trim_trailing_slash(s: &str) -> &str {
    s.trim_end_matches('/')
}

/// Percent-decode a URL-encoded path and return an absolute PathBuf.
/// Rejects relative paths — pa:// links should always carry an
/// absolute path so the meaning is unambiguous regardless of `$PWD`.
fn decoded_path(s: &str) -> Result<PathBuf> {
    let trimmed = trim_trailing_slash(s);
    if trimmed.is_empty() {
        return Err(anyhow!("empty path"));
    }
    let decoded = percent_decode(trimmed)?;
    // Paths from pa:// URLs are treated as absolute. The host side
    // of the scheme is used as the first path segment on Unix
    // (pa://open/home/u/... → /home/u/...), so prepend a `/` if
    // the decoded form doesn't already start with one and isn't a
    // drive-letter Windows path.
    let pb = if decoded.starts_with('/') || is_windows_absolute(&decoded) {
        PathBuf::from(decoded)
    } else {
        PathBuf::from(format!("/{decoded}"))
    };
    Ok(pb)
}

fn is_windows_absolute(s: &str) -> bool {
    // Matches "C:/..." or "C:\..." — drive letter + colon + sep.
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && matches!(chars.next(), Some(':'))
        && matches!(chars.next(), Some('/') | Some('\\'))
}

/// Tiny percent-decoder. Handles `%XX` → byte; rejects malformed.
/// Stdlib-only so we don't pull in a crate for a one-screen feature.
fn percent_decode(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            if i + 2 >= bytes.len() {
                return Err(anyhow!("truncated percent-escape at position {i}"));
            }
            let hi = hex_nibble(bytes[i + 1])?;
            let lo = hex_nibble(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else if b == b'+' {
            // Form-encoded style; some senders use it. Decode as space.
            out.push(b' ');
            i += 1;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).context("percent-decoded URL isn't valid UTF-8")
}

fn hex_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(anyhow!("not a hex digit: {:?}", b as char)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_open_with_absolute_path() {
        let a = parse("pa://open/home/u/code/proj").unwrap();
        assert_eq!(a, ProtocolAction::Open(PathBuf::from("/home/u/code/proj")));
    }

    #[test]
    fn parses_open_with_percent_encoded_path() {
        let a = parse("pa://open/home/u/1%20Projects/proj").unwrap();
        assert_eq!(
            a,
            ProtocolAction::Open(PathBuf::from("/home/u/1 Projects/proj"))
        );
    }

    #[test]
    fn parses_open_with_trailing_slash() {
        let a = parse("pa://open/tmp/x/").unwrap();
        assert_eq!(a, ProtocolAction::Open(PathBuf::from("/tmp/x")));
    }

    #[test]
    fn parses_shell_variant() {
        let a = parse("pa://shell/home/u/x").unwrap();
        assert_eq!(a, ProtocolAction::Shell(PathBuf::from("/home/u/x")));
    }

    #[test]
    fn parses_windows_drive_path() {
        // pa://open/C:/Users/X/code/proj → C:/Users/X/code/proj
        // Percent-encode the colon so the URL parses cleanly.
        let a = parse("pa://open/C%3A/Users/X/code/proj").unwrap();
        assert_eq!(
            a,
            ProtocolAction::Open(PathBuf::from("C:/Users/X/code/proj"))
        );
    }

    #[test]
    fn parses_workspace_by_id() {
        let a = parse("pa://workspace/abc-123-def").unwrap();
        assert_eq!(a, ProtocolAction::WorkspaceById("abc-123-def".into()));
    }

    #[test]
    fn parses_launch_with_session() {
        let a = parse("pa://launch/abc-123/claude").unwrap();
        match a {
            ProtocolAction::LaunchSession {
                workspace_id,
                session,
            } => {
                assert_eq!(workspace_id, "abc-123");
                assert_eq!(session, "claude");
            }
            other => panic!("expected LaunchSession, got {other:?}"),
        }
    }

    #[test]
    fn launch_session_decodes_percent_in_name() {
        let a = parse("pa://launch/abc/my%20session").unwrap();
        match a {
            ProtocolAction::LaunchSession { session, .. } => {
                assert_eq!(session, "my session");
            }
            other => panic!("expected LaunchSession, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_pa_scheme() {
        let err = parse("http://example.com/").unwrap_err();
        assert!(err.to_string().contains("not a pa://"));
    }

    #[test]
    fn rejects_unknown_action() {
        let err = parse("pa://explode/boom").unwrap_err();
        assert!(err.to_string().contains("unknown"));
    }

    #[test]
    fn rejects_empty_action() {
        let err = parse("pa://").unwrap_err();
        assert!(err.to_string().contains("missing an action"));
    }

    #[test]
    fn rejects_workspace_without_id() {
        let err = parse("pa://workspace/").unwrap_err();
        assert!(err.to_string().contains("requires a workspace id"));
    }

    #[test]
    fn rejects_launch_missing_session() {
        let err = parse("pa://launch/abc").unwrap_err();
        assert!(err.to_string().contains("requires both"));
    }

    #[test]
    fn rejects_launch_missing_id() {
        let err = parse("pa://launch//claude").unwrap_err();
        assert!(err.to_string().contains("requires both"));
    }

    #[test]
    fn percent_decodes_plus_as_space() {
        let a = parse("pa://launch/abc/my+session").unwrap();
        match a {
            ProtocolAction::LaunchSession { session, .. } => {
                assert_eq!(session, "my session");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn rejects_malformed_percent_escape() {
        let err = parse("pa://open/home/x%zz").unwrap_err();
        // anyhow chain: "decoding open path" → "not a hex digit"
        let full = format!("{err:#}");
        assert!(
            full.to_lowercase().contains("hex"),
            "expected hex mention in error chain, got: {full:?}"
        );
    }
}
