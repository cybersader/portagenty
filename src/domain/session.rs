//! A single executable unit in a workspace — `name + cwd + command`.
//!
//! See `DESIGN.md` §1 for the definition. v1's schema is deliberately
//! minimal; env vars, pre/post commands, profile references are still
//! v1.x extensions. `kind` arrives in v1.x as a display-only hint
//! (ROADMAP v1.x item 9); future releases may wire smart-resume or
//! agent-specific launch tweaks off of it, but for now it only
//! affects how a row renders in the TUI.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Optional hint about what kind of thing a session runs. Purely
/// informational in v1.x — the TUI uses it to render a small marker
/// so users can see at a glance which row is an agent vs a dev
/// server vs a plain shell. Serialized as a kebab-case string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionKind {
    /// Anthropic's Claude Code CLI (`claude`).
    ClaudeCode,
    /// sst/opencode CLI.
    Opencode,
    /// A plain interactive shell.
    Shell,
    /// An editor (vim, nvim, helix, etc.) or IDE TUI.
    Editor,
    /// A dev server or long-running process (webpack, vite, watch, etc.).
    DevServer,
    /// Anything else.
    Other,
}

impl SessionKind {
    /// One-letter marker for the TUI. `None` for Shell/Other —
    /// those are so generic that no marker is clearer than a marker.
    pub fn marker(&self) -> Option<char> {
        match self {
            SessionKind::ClaudeCode => Some('C'),
            SessionKind::Opencode => Some('O'),
            SessionKind::Editor => Some('E'),
            SessionKind::DevServer => Some('D'),
            SessionKind::Shell | SessionKind::Other => None,
        }
    }

    /// Human-readable label, used in debug contexts + tests.
    pub fn label(&self) -> &'static str {
        match self {
            SessionKind::ClaudeCode => "claude-code",
            SessionKind::Opencode => "opencode",
            SessionKind::Shell => "shell",
            SessionKind::Editor => "editor",
            SessionKind::DevServer => "dev-server",
            SessionKind::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    /// Resolved absolute working directory. Config-layer loaders resolve
    /// `~`, `${VAR}`, and relative-to-file paths before a `Session` leaves
    /// the config module; downstream consumers never see a relative path.
    pub cwd: PathBuf,
    pub command: String,
    /// Optional kind hint. When absent, the TUI treats the session as
    /// generic and omits the kind marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<SessionKind>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trip_without_kind() {
        let s = Session {
            name: "claude".into(),
            cwd: PathBuf::from("/home/user/code/portagenty"),
            command: "claude".into(),
            kind: None,
        };
        let encoded = toml::to_string(&s).expect("serialize");
        assert!(
            !encoded.contains("kind"),
            "kind should be skipped when None: {encoded}"
        );
        let decoded: Session = toml::from_str(&encoded).expect("deserialize");
        assert_eq!(s, decoded);
    }

    #[test]
    fn serde_round_trip_with_kind() {
        for (kind, wire) in [
            (SessionKind::ClaudeCode, "claude-code"),
            (SessionKind::Opencode, "opencode"),
            (SessionKind::Editor, "editor"),
            (SessionKind::DevServer, "dev-server"),
            (SessionKind::Shell, "shell"),
            (SessionKind::Other, "other"),
        ] {
            let s = Session {
                name: "s".into(),
                cwd: PathBuf::from("/tmp"),
                command: "c".into(),
                kind: Some(kind),
            };
            let encoded = toml::to_string(&s).unwrap();
            assert!(
                encoded.contains(wire),
                "expected wire form {wire:?} in:\n{encoded}"
            );
            let decoded: Session = toml::from_str(&encoded).unwrap();
            assert_eq!(decoded.kind, Some(kind));
        }
    }

    #[test]
    fn kind_absent_in_toml_deserializes_as_none() {
        let src = r#"
name = "claude"
cwd = "/tmp"
command = "claude"
"#;
        let s: Session = toml::from_str(src).unwrap();
        assert_eq!(s.kind, None);
    }

    #[test]
    fn markers_are_unique_among_marker_kinds() {
        let kinds = [
            SessionKind::ClaudeCode,
            SessionKind::Opencode,
            SessionKind::Editor,
            SessionKind::DevServer,
        ];
        let markers: Vec<char> = kinds.iter().filter_map(|k| k.marker()).collect();
        let uniq: std::collections::HashSet<char> = markers.iter().copied().collect();
        assert_eq!(
            markers.len(),
            uniq.len(),
            "markers should be unique: {markers:?}"
        );
    }

    #[test]
    fn shell_and_other_have_no_marker() {
        assert_eq!(SessionKind::Shell.marker(), None);
        assert_eq!(SessionKind::Other.marker(), None);
    }
}
