//! A `Workspace` — the merged, resolved view of what a user is currently
//! working on. Produced by `config::load` after combining the global,
//! workspace-file, and per-project tiers (DESIGN §2).
//!
//! `Workspace` is the value everything downstream (TUI, mux, CLI) consumes.
//! It holds absolute paths only; no `~`, no `${VAR}`, no relative-to-file.

use crate::domain::session::Session;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which multiplexer runs a workspace's sessions. v1 ships tmux only; the
/// other two variants deserialize successfully so a workspace file can pin
/// a future mpx without a parse error, and an attempt to actually launch
/// surfaces a clear "adapter not available" message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Multiplexer {
    #[default]
    Tmux,
    Zellij,
    Wezterm,
}

/// Fully-merged workspace value. Not a direct 1:1 of any on-disk file — it's
/// what callers of `config::load` get back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,

    /// The workspace file this was loaded from, if any. `None` when the
    /// workspace was synthesized from the global registry + per-project
    /// tier with no explicit `*.portagenty.toml` in the mix.
    #[serde(default)]
    pub file_path: Option<PathBuf>,

    /// Resolved multiplexer choice. Precedence: workspace file override →
    /// global default → `Multiplexer::default()` (tmux).
    pub multiplexer: Multiplexer,

    /// Absolute paths to the projects this workspace includes. Order is
    /// preserved from the workspace file's declaration.
    #[serde(default)]
    pub projects: Vec<PathBuf>,

    /// Sessions this workspace knows about. Already merged across tiers
    /// per DESIGN §2: workspace-level beats per-project beats global. Session
    /// `cwd` is absolute.
    #[serde(default)]
    pub sessions: Vec<Session>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiplexer_serde_is_kebab_case() {
        // Embed the enum in a table so TOML has a top-level table to write.
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            mpx: Multiplexer,
        }

        for (variant, expected) in [
            (Multiplexer::Tmux, "mpx = \"tmux\""),
            (Multiplexer::Zellij, "mpx = \"zellij\""),
            (Multiplexer::Wezterm, "mpx = \"wezterm\""),
        ] {
            let s = toml::to_string(&Wrap { mpx: variant }).unwrap();
            assert!(s.contains(expected), "expected {expected:?} in {s:?}");
            let back: Wrap = toml::from_str(&s).unwrap();
            assert_eq!(back.mpx, variant);
        }
    }

    #[test]
    fn multiplexer_default_is_tmux() {
        assert_eq!(Multiplexer::default(), Multiplexer::Tmux);
    }

    #[test]
    fn workspace_round_trip_minimal() {
        let w = Workspace {
            name: "Agentic stuff".into(),
            file_path: None,
            multiplexer: Multiplexer::Tmux,
            projects: vec![],
            sessions: vec![],
        };
        let encoded = toml::to_string(&w).unwrap();
        let decoded: Workspace = toml::from_str(&encoded).unwrap();
        assert_eq!(w, decoded);
    }

    #[test]
    fn workspace_round_trip_with_sessions() {
        let w = Workspace {
            name: "Agentic stuff".into(),
            file_path: Some(PathBuf::from("/home/u/ws/agentic.portagenty.toml")),
            multiplexer: Multiplexer::Tmux,
            projects: vec![PathBuf::from("/home/u/code/portagenty")],
            sessions: vec![Session {
                name: "claude".into(),
                cwd: PathBuf::from("/home/u/code/portagenty"),
                command: "claude".into(),
            }],
        };
        let encoded = toml::to_string(&w).unwrap();
        let decoded: Workspace = toml::from_str(&encoded).unwrap();
        assert_eq!(w, decoded);
    }

    #[test]
    fn projects_and_sessions_default_to_empty() {
        let src = r#"
name = "empty"
multiplexer = "tmux"
"#;
        let w: Workspace = toml::from_str(src).unwrap();
        assert!(w.projects.is_empty());
        assert!(w.sessions.is_empty());
        assert!(w.file_path.is_none());
    }
}
