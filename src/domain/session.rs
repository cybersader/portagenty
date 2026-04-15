//! A single executable unit in a workspace — `name + cwd + command`.
//!
//! See `DESIGN.md` §1 for the definition. v1's schema is deliberately
//! minimal; env vars, pre/post commands, profile references, and `kind:`
//! hints are all v1.x extensions.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    /// Resolved absolute working directory. Config-layer loaders resolve
    /// `~`, `${VAR}`, and relative-to-file paths before a `Session` leaves
    /// the config module; downstream consumers never see a relative path.
    pub cwd: PathBuf,
    pub command: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trip() {
        let s = Session {
            name: "claude".into(),
            cwd: PathBuf::from("/home/user/code/portagenty"),
            command: "claude".into(),
        };
        let encoded = toml::to_string(&s).expect("serialize");
        let decoded: Session = toml::from_str(&encoded).expect("deserialize");
        assert_eq!(s, decoded);
    }
}
