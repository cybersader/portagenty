//! A registered project — a directory portagenty knows about, plus any
//! sessions or tags attached to it.
//!
//! See `DESIGN.md` §1. Projects are registered at any of the three tiers
//! (global / workspace / per-project); the config-layer merge reconciles
//! them before constructing the domain value.

use crate::domain::session::Session;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Resolved absolute project root.
    pub path: PathBuf,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub sessions: Vec<Session>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trip_minimal() {
        let p = Project {
            path: PathBuf::from("/home/user/code/portagenty"),
            tags: vec![],
            sessions: vec![],
        };
        let encoded = toml::to_string(&p).expect("serialize");
        let decoded: Project = toml::from_str(&encoded).expect("deserialize");
        assert_eq!(p, decoded);
    }

    #[test]
    fn tags_default_to_empty_when_absent() {
        let toml_src = r#"path = "/home/user/code/portagenty""#;
        let p: Project = toml::from_str(toml_src).expect("deserialize");
        assert!(p.tags.is_empty());
        assert!(p.sessions.is_empty());
    }

    #[test]
    fn serde_round_trip_with_sessions() {
        let p = Project {
            path: PathBuf::from("/home/user/code/portagenty"),
            tags: vec!["rust".into(), "agentic".into()],
            sessions: vec![Session {
                name: "tests".into(),
                cwd: PathBuf::from("/home/user/code/portagenty"),
                command: "cargo nextest run".into(),
            }],
        };
        let encoded = toml::to_string(&p).expect("serialize");
        let decoded: Project = toml::from_str(&encoded).expect("deserialize");
        assert_eq!(p, decoded);
    }
}
