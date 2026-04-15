//! Three-tier config loader: global + workspace + per-project. See
//! `DESIGN.md` §2. Public API is finalized in a later commit once the
//! merge + discovery pieces land.

pub mod discovery;
pub mod files;

pub use discovery::{
    global_config_path, is_workspace_filename, project_file_in_dir, walk_up_from, workspace_in_dir,
};
pub use files::{
    load_toml, GlobalFile, GlobalProjectEntry, GlobalWorkspaceEntry, ProjectFile, RawSession,
    WorkspaceFile,
};
