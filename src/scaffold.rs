//! Single source of truth for "scaffold a workspace file." Three
//! call sites need to do this: `pa init`, the onboarding wizard, and
//! the in-TUI new-workspace flow (commit chain in
//! `~/.claude/plans/piped-sauteeing-breeze.md`). Pulling the file-
//! mutation core into one function avoids the drift we already had
//! between `cli::init` and `onboarding::scaffold_flow` — both
//! sanitized filenames, both rendered TOML bodies, both registered
//! globally, and the strings were already creeping apart.
//!
//! What this module does NOT do: any user prompting or stdout
//! writing. Callers handle their own I/O. This keeps the function
//! testable and reusable from a TUI overlay where stdio is captured.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::domain::Multiplexer;

/// Outcome of [`create_at`]. The variant tells the caller whether to
/// show "created" or "already existed; reusing" copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScaffoldOutcome {
    /// File was newly written at this path.
    Created(PathBuf),
    /// File already existed at this path; we left it alone.
    AlreadyExisted(PathBuf),
}

impl ScaffoldOutcome {
    /// The on-disk path of the workspace file, regardless of whether
    /// we wrote it just now or it was already there.
    pub fn path(&self) -> &Path {
        match self {
            ScaffoldOutcome::Created(p) | ScaffoldOutcome::AlreadyExisted(p) => p,
        }
    }
}

/// Scaffold a `*.portagenty.toml` in `target_dir`.
///
/// - `display_name` is the workspace's human-readable name as it'll
///   appear in the title bar. The on-disk filename is a sanitized
///   stem of this name.
/// - `mpx` is the multiplexer to pin in the file's `multiplexer`
///   field.
/// - `with_claude` adds a starter `claude` session alongside the
///   default `shell` session.
/// - `force` overwrites any existing workspace file at the target
///   path. When false, an existing file returns `AlreadyExisted`
///   without writing.
///
/// On success the new path is appended to the global workspace
/// registry (best-effort — a registry write failure is logged via
/// [`tracing::warn`] but does not fail the call, since the local
/// file still works via walk-up).
pub fn create_at(
    target_dir: &Path,
    display_name: &str,
    mpx: Multiplexer,
    with_claude: bool,
    force: bool,
) -> Result<ScaffoldOutcome> {
    if !target_dir.is_dir() {
        return Err(anyhow!(
            "scaffold target {} is not a directory",
            target_dir.display()
        ));
    }
    let stem = sanitize_filename_stem(display_name);
    if stem.is_empty() {
        return Err(anyhow!(
            "workspace name {display_name:?} sanitizes to an empty string"
        ));
    }
    let filename = format!("{stem}.portagenty.toml");
    let path = target_dir.join(filename);

    if path.exists() && !force {
        return Ok(ScaffoldOutcome::AlreadyExisted(path));
    }

    let body = render_toml_body(display_name, mpx, with_claude);
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;

    if let Err(e) = crate::config::register_global_workspace(&path) {
        tracing::warn!(
            target = "portagenty::scaffold",
            error = %e,
            path = %path.display(),
            "couldn't register scaffolded workspace in global index",
        );
    }

    Ok(ScaffoldOutcome::Created(path))
}

/// Map a workspace display name to a filename-safe stem. Replaces
/// any character that isn't ASCII alphanumeric or `-` / `_` with `_`.
/// Preserves multi-byte chars only by collapsing them to underscores
/// (we'd rather have a portable filename than a clever one).
pub fn sanitize_filename_stem(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Render the TOML body for a fresh workspace file. Header comment
/// links the schema docs; one default `shell` session is always
/// present; an optional `claude` (kind=claude-code) session lands at
/// the bottom when requested.
pub fn render_toml_body(display_name: &str, mpx: Multiplexer, with_claude: bool) -> String {
    let mpx_wire = match mpx {
        Multiplexer::Tmux => "tmux",
        Multiplexer::Zellij => "zellij",
        Multiplexer::Wezterm => "wezterm",
    };
    let name_lit = toml_basic_string(display_name);
    let id = uuid::Uuid::new_v4();
    let mut out = String::with_capacity(256);
    out.push_str(
        "# Workspace file for portagenty. See:\n\
         # https://cybersader.github.io/portagenty/reference/schema/\n",
    );
    out.push_str(&format!("name = {name_lit}\n"));
    out.push_str(&format!("id = \"{id}\"\n"));
    out.push_str(&format!("multiplexer = \"{mpx_wire}\"\n\n"));
    out.push_str(
        "[[session]]\nname = \"shell\"\ncwd = \".\"\ncommand = \"bash\"\nkind = \"shell\"\n",
    );
    if with_claude {
        out.push_str(
            "\n[[session]]\nname = \"claude\"\ncwd = \".\"\ncommand = \"claude\"\nkind = \"claude-code\"\n",
        );
    }
    out
}

/// Quote `s` as a TOML basic string. Escapes `\` and `"`; everything
/// else is passed through. Workspace names from the wizard or `pa
/// init` arrive as well-formed UTF-8, so we don't try to handle
/// control characters or unicode escapes.
fn toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::prelude::*;

    #[test]
    fn sanitize_keeps_alphanumeric_and_dash_underscore() {
        assert_eq!(sanitize_filename_stem("my-workspace_42"), "my-workspace_42");
    }

    #[test]
    fn sanitize_replaces_spaces_dots_and_slashes() {
        assert_eq!(
            sanitize_filename_stem("My Cool/Project.v2"),
            "My_Cool_Project_v2"
        );
    }

    #[test]
    fn sanitize_collapses_unicode_to_underscores() {
        // Single-codepoint emoji and accented chars each become `_`.
        // "café 🚀" = c + a + f + é + space + 🚀 = 3 ascii + 3 non-ascii.
        assert_eq!(sanitize_filename_stem("café 🚀"), "caf___");
    }

    #[test]
    fn render_body_includes_name_mpx_and_default_shell() {
        let body = render_toml_body("Demo", Multiplexer::Tmux, false);
        assert!(body.contains(r#"name = "Demo""#), "missing name: {body}");
        assert!(
            body.contains(r#"multiplexer = "tmux""#),
            "missing mpx: {body}"
        );
        assert!(
            body.contains(r#"name = "shell""#),
            "missing shell session: {body}"
        );
        assert!(
            !body.contains(r#"name = "claude""#),
            "claude should be absent without with_claude: {body}"
        );
    }

    #[test]
    fn render_body_includes_claude_when_requested() {
        let body = render_toml_body("Demo", Multiplexer::Zellij, true);
        assert!(
            body.contains(r#"name = "claude""#),
            "missing claude: {body}"
        );
        assert!(
            body.contains(r#"kind = "claude-code""#),
            "claude needs kind hint: {body}"
        );
        assert!(
            body.contains(r#"multiplexer = "zellij""#),
            "wrong mpx wire: {body}"
        );
    }

    #[test]
    fn render_body_escapes_quotes_in_name() {
        let body = render_toml_body(r#"with "quotes""#, Multiplexer::Tmux, false);
        assert!(
            body.contains(r#"name = "with \"quotes\"""#),
            "wrong escape: {body}"
        );
    }

    #[test]
    fn create_at_writes_file_and_returns_created() {
        let tmp = assert_fs::TempDir::new().unwrap();
        // Test isolation: ensure global registry write goes nowhere
        // visible by sandboxing XDG_CONFIG_HOME for this process.
        let _xdg = TempXdg::new();
        let outcome = create_at(tmp.path(), "demo", Multiplexer::Tmux, false, false).unwrap();
        assert!(matches!(outcome, ScaffoldOutcome::Created(_)));
        assert!(outcome.path().is_file());
        assert_eq!(outcome.path().file_name().unwrap(), "demo.portagenty.toml");
    }

    #[test]
    fn create_at_returns_already_existed_when_present_without_force() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let _xdg = TempXdg::new();
        tmp.child("demo.portagenty.toml")
            .write_str("name = \"old\"\nmultiplexer = \"tmux\"\n")
            .unwrap();
        let outcome = create_at(tmp.path(), "demo", Multiplexer::Tmux, false, false).unwrap();
        assert!(matches!(outcome, ScaffoldOutcome::AlreadyExisted(_)));
        // File contents should be unchanged.
        let contents = std::fs::read_to_string(outcome.path()).unwrap();
        assert!(contents.contains(r#"name = "old""#));
    }

    #[test]
    fn create_at_force_overwrites_existing() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let _xdg = TempXdg::new();
        tmp.child("demo.portagenty.toml")
            .write_str("name = \"old\"\n")
            .unwrap();
        let outcome = create_at(tmp.path(), "demo", Multiplexer::Tmux, false, true).unwrap();
        assert!(matches!(outcome, ScaffoldOutcome::Created(_)));
        let contents = std::fs::read_to_string(outcome.path()).unwrap();
        assert!(contents.contains(r#"name = "demo""#));
    }

    #[test]
    fn create_at_errors_when_target_dir_missing() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let _xdg = TempXdg::new();
        let nope = tmp.path().join("does-not-exist");
        let err = create_at(&nope, "demo", Multiplexer::Tmux, false, false).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "got: {err}");
    }

    #[test]
    fn create_at_errors_when_name_sanitizes_to_empty() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let _xdg = TempXdg::new();
        // A pure-emoji name collapses to all underscores… actually
        // that's not empty. Use a name made of only chars that DON'T
        // map: empty input itself.
        let err = create_at(tmp.path(), "", Multiplexer::Tmux, false, false).unwrap_err();
        assert!(
            err.to_string().contains("empty string"),
            "expected empty-stem error, got: {err}"
        );
    }

    /// RAII guard pinning a tempdir as XDG_CONFIG_HOME so the
    /// registry write inside `create_at` doesn't leak into the real
    /// user config during tests.
    struct TempXdg {
        _dir: assert_fs::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl TempXdg {
        fn new() -> Self {
            let dir = assert_fs::TempDir::new().unwrap();
            let previous = std::env::var_os("XDG_CONFIG_HOME");
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            Self {
                _dir: dir,
                previous,
            }
        }
    }

    impl Drop for TempXdg {
        fn drop(&mut self) {
            match &self.previous {
                Some(p) => std::env::set_var("XDG_CONFIG_HOME", p),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }
}
