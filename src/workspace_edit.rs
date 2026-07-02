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

/// Update the `name = "..."` field. Preserves comments and unrelated
/// fields via toml_edit. Errors if the new name is empty or if the
/// file isn't valid TOML.
pub fn set_name(workspace_file: &Path, new_name: &str) -> Result<()> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("workspace name cannot be empty"));
    }
    let raw = std::fs::read_to_string(workspace_file)
        .with_context(|| format!("reading {}", workspace_file.display()))?;
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing {}", workspace_file.display()))?;
    doc["name"] = toml_edit::value(trimmed);
    std::fs::write(workspace_file, doc.to_string())
        .with_context(|| format!("writing {}", workspace_file.display()))?;
    Ok(())
}

/// Replace the workspace's `tags = [...]` array. Trims + drops empty
/// entries and de-duplicates (order-preserving). An empty result
/// removes the `tags` key entirely so the file stays tidy. Preserves
/// comments + unrelated fields via toml_edit.
pub fn set_tags(workspace_file: &Path, tags: &[String]) -> Result<()> {
    let raw = std::fs::read_to_string(workspace_file)
        .with_context(|| format!("reading {}", workspace_file.display()))?;
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing {}", workspace_file.display()))?;

    let mut seen = std::collections::HashSet::new();
    let cleaned: Vec<String> = tags
        .iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty() && seen.insert(t.clone()))
        .collect();

    if cleaned.is_empty() {
        doc.remove("tags");
    } else {
        let mut arr = toml_edit::Array::new();
        for t in &cleaned {
            arr.push(t.as_str());
        }
        doc["tags"] = toml_edit::value(arr);
    }
    std::fs::write(workspace_file, doc.to_string())
        .with_context(|| format!("writing {}", workspace_file.display()))?;
    Ok(())
}

/// Parse a comma-separated tag input string into a clean tag list.
/// Splits on `,`, trims, drops empties. Used by the picker's tag
/// editor modal.
pub fn parse_tags_input(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::prelude::*;

    #[test]
    fn set_tags_writes_deduped_array_and_preserves_fields() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let p = tmp.child("ws.portagenty.toml");
        p.write_str(
            "# c\nname = \"demo\"\nmultiplexer = \"tmux\"\n\n[[session]]\nname = \"shell\"\ncwd = \".\"\ncommand = \"bash\"\n",
        )
        .unwrap();
        set_tags(
            p.path(),
            &["rust".into(), " agentic ".into(), "rust".into(), "".into()],
        )
        .unwrap();
        let raw = std::fs::read_to_string(p.path()).unwrap();
        assert!(raw.contains(r#"tags = ["rust", "agentic"]"#), "raw: {raw}");
        assert!(raw.contains("# c"), "comment lost: {raw}");
        assert!(raw.contains(r#"name = "shell""#), "session lost: {raw}");
    }

    #[test]
    fn set_tags_empty_removes_key() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let p = tmp.child("ws.portagenty.toml");
        p.write_str("name = \"demo\"\ntags = [\"old\"]\n").unwrap();
        set_tags(p.path(), &[]).unwrap();
        let raw = std::fs::read_to_string(p.path()).unwrap();
        assert!(!raw.contains("tags"), "tags not removed: {raw}");
    }

    #[test]
    fn parse_tags_input_splits_and_trims() {
        assert_eq!(
            parse_tags_input(" rust , agentic ,, tui "),
            vec!["rust", "agentic", "tui"]
        );
        assert!(parse_tags_input("  ,  ").is_empty());
    }

    #[test]
    fn set_name_updates_and_preserves_other_fields() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let p = tmp.child("ws.portagenty.toml");
        p.write_str(
            "# my comment\n\
             name = \"old-name\"\n\
             multiplexer = \"tmux\"\n\
             \n\
             [[session]]\n\
             name = \"shell\"\n\
             cwd = \".\"\n\
             command = \"bash\"\n",
        )
        .unwrap();

        set_name(p.path(), "new-name").unwrap();
        let raw = std::fs::read_to_string(p.path()).unwrap();
        assert!(raw.contains("name = \"new-name\""), "raw: {raw}");
        assert!(!raw.contains("old-name"), "old name still present: {raw}");
        assert!(raw.contains("# my comment"), "comment lost: {raw}");
        assert!(
            raw.contains("name = \"shell\""),
            "session block lost: {raw}"
        );
    }

    #[test]
    fn set_name_rejects_empty() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let p = tmp.child("ws.portagenty.toml");
        p.write_str("name = \"demo\"\n").unwrap();
        let err = set_name(p.path(), "   ").unwrap_err();
        assert!(err.to_string().contains("empty"), "got: {err}");
    }

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
