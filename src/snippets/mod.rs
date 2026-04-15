//! Bundled bash snippets that users can install into their rc files.
//! The idea: `pa` stays lean in its core, but ships opinionated
//! defaults that align with the agentic-workflow use case a user
//! can opt into with one command.
//!
//! Each snippet is a static bash file compiled into the binary via
//! `include_str!`. Install wraps it in `BEGIN`/`END` markers so
//! subsequent runs cleanly replace the block instead of duplicating
//! or leaving stale text behind.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// One distributable snippet. `description` is the short line that
/// shows up in `pa snippets list`.
#[derive(Debug, Clone, Copy)]
pub struct Snippet {
    pub name: &'static str,
    pub description: &'static str,
    pub contents: &'static str,
}

/// The catalog, ordered for UX (most-useful first).
pub const SNIPPETS: &[Snippet] = &[
    Snippet {
        name: "pa-aliases",
        description: "short bash aliases for pa commands (p, pl, pc, pls, pe, pi, pad)",
        contents: include_str!("../../snippets/pa-aliases.sh"),
    },
    Snippet {
        name: "termux-friendly",
        description: "mobile-SSH ergonomics: flow-control fix, key-bindings, cc/ccc aliases",
        contents: include_str!("../../snippets/termux-friendly.sh"),
    },
];

/// Look up a snippet by name. Returns a clear error listing every
/// available snippet if the name doesn't match.
pub fn lookup(name: &str) -> Result<&'static Snippet> {
    SNIPPETS.iter().find(|s| s.name == name).ok_or_else(|| {
        let available: Vec<&str> = SNIPPETS.iter().map(|s| s.name).collect();
        anyhow!(
            "no snippet named {name:?}. available: {}",
            available.join(", ")
        )
    })
}

/// Default rc file: `$HOME/.bashrc` on Unix, `$USERPROFILE/.bashrc`
/// on Windows (user can override with --to).
pub fn default_rcfile() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| anyhow!("neither HOME nor USERPROFILE is set"))?;
    Ok(PathBuf::from(home).join(".bashrc"))
}

/// Marker lines that surround a snippet block in an rc file. The
/// name is embedded in the marker so multiple snippets can live in
/// the same file without stepping on each other.
fn begin_marker(name: &str) -> String {
    format!("# >>> pa snippet: {name} >>>")
}

fn end_marker(name: &str) -> String {
    format!("# <<< pa snippet: {name} <<<")
}

/// Build the complete block we'd write to an rc file, including the
/// header comment and markers.
pub fn render_block(snippet: &Snippet) -> String {
    let begin = begin_marker(snippet.name);
    let end = end_marker(snippet.name);
    format!(
        "{begin}\n# Installed by `pa snippets install {name}`. Do not edit by hand —\n# re-run the install command to update, or `pa snippets uninstall {name}`.\n{contents}{trailing}{end}\n",
        name = snippet.name,
        contents = snippet.contents,
        trailing = if snippet.contents.ends_with('\n') { "" } else { "\n" },
    )
}

/// Compute the new rc-file contents after installing `snippet`. Pure
/// function (no I/O) so tests can drive it against in-memory input.
pub fn install_into(existing: &str, snippet: &Snippet) -> String {
    let begin = begin_marker(snippet.name);
    let end = end_marker(snippet.name);
    let block = render_block(snippet);

    if let (Some(b_idx), Some(e_offset)) = (existing.find(&begin), existing.find(&end)) {
        // `end` appears after `begin`; replace the entire inclusive
        // block (plus any trailing newline on the end marker line).
        let e_idx = e_offset + end.len();
        let e_idx = if existing.as_bytes().get(e_idx) == Some(&b'\n') {
            e_idx + 1
        } else {
            e_idx
        };
        let mut out = String::with_capacity(existing.len() + block.len());
        out.push_str(&existing[..b_idx]);
        out.push_str(&block);
        out.push_str(&existing[e_idx..]);
        out
    } else {
        // Append a fresh block. Ensure exactly one blank line of
        // separation from whatever was there before.
        let mut out = String::with_capacity(existing.len() + block.len() + 2);
        out.push_str(existing);
        if !existing.is_empty() && !existing.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&block);
        out
    }
}

/// Compute the new rc-file contents after uninstalling `name`.
/// Pure: no I/O.
pub fn uninstall_from(existing: &str, name: &str) -> Option<String> {
    let begin = begin_marker(name);
    let end = end_marker(name);

    let b_idx = existing.find(&begin)?;
    let e_offset = existing.find(&end)?;
    if e_offset < b_idx {
        return None;
    }
    let e_idx = e_offset + end.len();
    let e_idx = if existing.as_bytes().get(e_idx) == Some(&b'\n') {
        e_idx + 1
    } else {
        e_idx
    };

    // Also eat one trailing blank line if the block was preceded by one,
    // to avoid gradual blank-line accumulation on install → uninstall
    // cycles.
    let start = if b_idx > 0 && existing.as_bytes()[b_idx - 1] == b'\n' {
        // Preserve exactly one newline before wherever we resume.
        b_idx
    } else {
        b_idx
    };

    let mut out = String::with_capacity(existing.len());
    out.push_str(&existing[..start]);
    out.push_str(&existing[e_idx..]);
    Some(out)
}

/// Install `snippet` into `rcfile`. Creates the file if it doesn't
/// exist. Returns the new contents it wrote, so the caller can show
/// the user a before/after if they want.
pub fn install(rcfile: &Path, snippet: &Snippet) -> Result<String> {
    let existing = std::fs::read_to_string(rcfile).unwrap_or_default();
    let new = install_into(&existing, snippet);
    if let Some(parent) = rcfile.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(rcfile, &new).with_context(|| format!("writing {}", rcfile.display()))?;
    Ok(new)
}

/// Remove the snippet from `rcfile`. Returns `None` if the snippet
/// wasn't installed.
pub fn uninstall(rcfile: &Path, name: &str) -> Result<Option<String>> {
    let existing =
        std::fs::read_to_string(rcfile).with_context(|| format!("reading {}", rcfile.display()))?;
    let Some(new) = uninstall_from(&existing, name) else {
        return Ok(None);
    };
    std::fs::write(rcfile, &new).with_context(|| format!("writing {}", rcfile.display()))?;
    Ok(Some(new))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pa_aliases() -> &'static Snippet {
        lookup("pa-aliases").unwrap()
    }

    #[test]
    fn catalog_is_non_empty_and_every_entry_has_content() {
        assert!(!SNIPPETS.is_empty(), "snippet catalog shouldn't be empty");
        for s in SNIPPETS {
            assert!(!s.name.is_empty(), "snippet name empty");
            assert!(!s.description.is_empty(), "snippet description empty");
            assert!(!s.contents.is_empty(), "snippet contents empty: {s:?}");
        }
    }

    #[test]
    fn snippet_names_are_unique() {
        let names: Vec<&str> = SNIPPETS.iter().map(|s| s.name).collect();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(
            names.len(),
            unique.len(),
            "duplicate snippet names in catalog"
        );
    }

    #[test]
    fn lookup_returns_matching_snippet() {
        let s = lookup("pa-aliases").unwrap();
        assert_eq!(s.name, "pa-aliases");
    }

    #[test]
    fn lookup_errors_on_unknown_name_and_lists_available() {
        let err = lookup("nope").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no snippet"));
        for s in SNIPPETS {
            assert!(msg.contains(s.name), "error should list {}: {msg}", s.name);
        }
    }

    #[test]
    fn install_into_empty_file_appends_block_with_markers() {
        let result = install_into("", pa_aliases());
        assert!(result.contains(&begin_marker("pa-aliases")));
        assert!(result.contains(&end_marker("pa-aliases")));
        assert!(result.contains("alias p='pa'"));
    }

    #[test]
    fn install_into_file_with_existing_content_appends_not_replaces() {
        let existing = "# my own bashrc\nexport FOO=bar\n";
        let result = install_into(existing, pa_aliases());
        assert!(
            result.starts_with(existing),
            "should preserve prior content"
        );
        assert!(result.contains("pa snippet: pa-aliases"));
    }

    #[test]
    fn install_into_is_idempotent() {
        let once = install_into("", pa_aliases());
        let twice = install_into(&once, pa_aliases());
        assert_eq!(once, twice, "second install should be a no-op");
    }

    #[test]
    fn install_into_updates_in_place_when_contents_change() {
        // Simulate an older version of the snippet by installing
        // a fake one, then reinstalling the real one.
        let fake = Snippet {
            name: "pa-aliases",
            description: "old",
            contents: "alias old='echo old'\n",
        };
        let after_old = install_into("", &fake);
        assert!(after_old.contains("alias old='echo old'"));

        let real = pa_aliases();
        let after_real = install_into(&after_old, real);
        assert!(
            !after_real.contains("alias old='echo old'"),
            "old contents should be gone: {after_real}"
        );
        assert!(
            after_real.contains("alias p='pa'"),
            "new contents should appear: {after_real}"
        );
        // Only one pair of markers for this snippet.
        assert_eq!(
            after_real.matches("pa snippet: pa-aliases").count(),
            2, // one begin, one end
            "should have exactly one begin+end, not duplicate\n{after_real}"
        );
    }

    #[test]
    fn uninstall_from_removes_block_cleanly() {
        let installed = install_into("export FOO=bar\n", pa_aliases());
        let result = uninstall_from(&installed, "pa-aliases").unwrap();
        assert!(!result.contains("pa snippet: pa-aliases"));
        assert!(result.contains("export FOO=bar"));
    }

    #[test]
    fn uninstall_from_returns_none_when_not_installed() {
        let input = "export FOO=bar\n";
        assert!(uninstall_from(input, "pa-aliases").is_none());
    }

    #[test]
    fn install_uninstall_roundtrip_preserves_user_content() {
        let user = "# pre-existing\nexport FOO=bar\n";
        let installed = install_into(user, pa_aliases());
        let uninstalled = uninstall_from(&installed, "pa-aliases").unwrap();
        assert!(uninstalled.contains("export FOO=bar"));
        assert!(!uninstalled.contains("pa snippet"));
    }

    #[test]
    fn render_block_contains_full_contents_and_markers() {
        let block = render_block(pa_aliases());
        assert!(block.starts_with(&begin_marker("pa-aliases")));
        assert!(block.contains("Installed by `pa snippets install pa-aliases`"));
        assert!(block.trim_end().ends_with(&end_marker("pa-aliases")));
    }

    #[test]
    fn default_rcfile_resolves_to_home_bashrc() {
        std::env::set_var("HOME", "/home/test");
        let p = default_rcfile().unwrap();
        assert_eq!(p, PathBuf::from("/home/test/.bashrc"));
    }
}
