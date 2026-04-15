//! Three-tier config loader: global + workspace + per-project. See
//! `DESIGN.md` §2. Public API is finalized in a later commit once the
//! merge + discovery pieces land.

pub mod files;

pub use files::{
    load_toml, GlobalFile, GlobalProjectEntry, GlobalWorkspaceEntry, ProjectFile, RawSession,
    WorkspaceFile,
};
