//! Core domain types — the shapes everything else in the crate passes
//! around. See `DESIGN.md` §1 for definitions.
//!
//! These are deliberately small in v1: a `Session` is `name + cwd +
//! command`, a `Project` is `path + tags + sessions`, and a `Workspace`
//! is the merged view produced by `crate::config::load`.

pub mod project;
pub mod session;
pub mod workspace;

pub use project::Project;
pub use session::Session;
pub use workspace::{Multiplexer, Workspace};
