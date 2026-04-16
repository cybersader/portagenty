//! Session-name sanitization for multiplexers. See `DESIGN.md` §5.
//!
//! Matches the VS Code extension's approach so sessions created there
//! are discoverable under the same sanitized name: non-alphanumeric /
//! non-underscore / non-hyphen chars get replaced with `_`, and the
//! result is clamped to 50 chars.

const MAX_LEN: usize = 50;

/// Sanitize a session name into a form every mpx accepts. Idempotent:
/// passing already-sanitized input returns the same string.
pub fn sanitize_session_name(raw: &str) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| if is_safe(c) { c } else { '_' })
        .collect();

    if mapped.chars().count() > MAX_LEN {
        mapped.chars().take(MAX_LEN).collect()
    } else {
        mapped
    }
}

/// Produce a workspace-scoped mpx session name. Prevents collisions
/// between workspaces that declare sessions with the same name
/// (e.g. both have `"shell"`). The mpx sees `"cyberchaste-shell"`;
/// the TUI display name stays unprefixed.
pub fn workspace_session_name(workspace_name: &str, session_name: &str) -> String {
    let prefix = sanitize_session_name(workspace_name);
    let suffix = sanitize_session_name(session_name);
    let combined = format!("{prefix}-{suffix}");
    if combined.chars().count() > MAX_LEN {
        combined.chars().take(MAX_LEN).collect()
    } else {
        combined
    }
}

fn is_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_alphanumeric_passes_through() {
        assert_eq!(sanitize_session_name("claude"), "claude");
        assert_eq!(sanitize_session_name("Session-42"), "Session-42");
        assert_eq!(sanitize_session_name("a_b-c"), "a_b-c");
    }

    #[test]
    fn spaces_become_underscores() {
        assert_eq!(sanitize_session_name("my cool task"), "my_cool_task");
    }

    #[test]
    fn tmux_index_format_is_rewritten() {
        // tmux sometimes reports session names with colons in window
        // indexes (e.g. "foo:0:1"). Our canonical form underscores them.
        assert_eq!(sanitize_session_name("foo:0:1"), "foo_0_1");
    }

    #[test]
    fn unicode_and_symbols_become_underscores() {
        assert_eq!(sanitize_session_name("café"), "caf_");
        assert_eq!(sanitize_session_name("💥boom💥"), "_boom_");
        assert_eq!(sanitize_session_name("a.b.c"), "a_b_c");
    }

    #[test]
    fn long_names_are_clamped_to_fifty_chars() {
        let long = "a".repeat(200);
        let out = sanitize_session_name(&long);
        assert_eq!(out.chars().count(), 50);
        assert!(out.chars().all(|c| c == 'a'));
    }

    #[test]
    fn exactly_fifty_chars_is_preserved() {
        let fifty = "a".repeat(50);
        assert_eq!(sanitize_session_name(&fifty), fifty);
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(sanitize_session_name(""), "");
    }

    #[test]
    fn is_idempotent() {
        for input in ["claude", "a b c", "café", "💥💥", &"x".repeat(200)] {
            let once = sanitize_session_name(input);
            let twice = sanitize_session_name(&once);
            assert_eq!(once, twice, "not idempotent for {input:?}");
        }
    }
}
