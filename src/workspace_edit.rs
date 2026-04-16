//! Workspace-level edits to a `*.portagenty.toml` file. Mirrors
//! `crate::cli::remove_session_from_file` in spirit — pure
//! file-mutation core with no stdio, callable from both CLI and
//! TUI without reaching into `cli/`.
//!
//! Comments and unrelated fields are preserved via toml_edit, the
//! same way `register_global_workspace` and `pa edit <session>`
//! handle their writes.

use std::path::Path;

use anyhow::{anyhow, Context, Result};

use crate::domain::Multiplexer;

/// Update the `multiplexer = "..."` field at the top of the
/// workspace file. Adds the field if it wasn't present (workspaces
/// can omit it to inherit the global default; switching pins it
/// explicitly). Errors if the file isn't valid TOML.
pub fn set_multiplexer(workspace_file: &Path, mpx: Multiplexer) -> Result<()> {
    let raw = std::fs::read_to_string(workspace_file)
        .with_context(|| format!("reading {}", workspace_file.display()))?;
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing {}", workspace_file.display()))?;
    let wire = match mpx {
        Multiplexer::Tmux => "tmux",
        Multiplexer::Zellij => "zellij",
        Multiplexer::Wezterm => {
            return Err(anyhow!(
                "wezterm isn't supported by portagenty; use tmux or zellij"
            ));
        }
    };
    doc["multiplexer"] = toml_edit::value(wire);
    std::fs::write(workspace_file, doc.to_string())
        .with_context(|| format!("writing {}", workspace_file.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::prelude::*;

    #[test]
    fn switches_tmux_to_zellij_and_preserves_other_fields() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let p = tmp.child("ws.portagenty.toml");
        p.write_str(
            "# my comment\n\
             name = \"demo\"\n\
             multiplexer = \"tmux\"\n\
             \n\
             [[session]]\n\
             name = \"shell\"\n\
             cwd = \".\"\n\
             command = \"bash\"\n",
        )
        .unwrap();

        set_multiplexer(p.path(), Multiplexer::Zellij).unwrap();
        let raw = std::fs::read_to_string(p.path()).unwrap();
        assert!(raw.contains("multiplexer = \"zellij\""), "raw: {raw}");
        assert!(raw.contains("# my comment"), "comment lost: {raw}");
        assert!(
            raw.contains("name = \"shell\""),
            "session block lost: {raw}"
        );
    }

    #[test]
    fn adds_field_when_workspace_didnt_pin_one() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let p = tmp.child("ws.portagenty.toml");
        p.write_str(
            "name = \"demo\"\n\
             \n\
             [[session]]\n\
             name = \"shell\"\n\
             cwd = \".\"\n\
             command = \"bash\"\n",
        )
        .unwrap();

        set_multiplexer(p.path(), Multiplexer::Zellij).unwrap();
        let raw = std::fs::read_to_string(p.path()).unwrap();
        assert!(raw.contains("multiplexer = \"zellij\""), "raw: {raw}");
    }

    #[test]
    fn rejects_wezterm() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let p = tmp.child("ws.portagenty.toml");
        p.write_str("name = \"demo\"\n").unwrap();
        let err = set_multiplexer(p.path(), Multiplexer::Wezterm).unwrap_err();
        assert!(err.to_string().contains("wezterm"), "got: {err}");
    }
}
