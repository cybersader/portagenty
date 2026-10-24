//! Launch-local Claude labels and environment isolation. Never changes logical
//! session names, multiplexer targets, persisted configuration, or routing vars.

use crate::domain::{Session, SessionKind};

const INHERITED_SESSION_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_PID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_DISABLE_BG_EXIT_HANDOFF",
    "AGENT_GUARD_WORKER_SCOPE",
    "AGENT_GUARD_INHERITED_SUBAGENT_MODEL",
];

pub(crate) fn prepare_session(workspace: &str, session: &Session) -> Session {
    let mut prepared = session.clone();
    if session.kind != Some(SessionKind::ClaudeCode) {
        return prepared;
    }

    let command = named_command(&session.command, &session_label(workspace, &session.name));
    // Clean at the workload boundary, not just on pa's multiplexer client:
    // existing tmux/Zellij servers may hold their own inherited environment.
    // A quoted Bash payload preserves custom commands and the command grammar
    // already used by the Zellij adapter and supervised workload anchor.
    let unset = INHERITED_SESSION_MARKERS
        .iter()
        .map(|name| format!(" -u {name}"))
        .collect::<String>();
    prepared.command = format!("env{unset} bash -c {}", shell_quote(&command));
    prepared
}

fn session_label(workspace: &str, session: &str) -> String {
    fn slug(value: &str, fallback: &str) -> String {
        let mut result = String::new();
        let mut separator = false;
        for ch in value.chars() {
            if ch.is_ascii_alphanumeric() {
                if separator && !result.is_empty() {
                    result.push('-');
                }
                result.push(ch.to_ascii_lowercase());
                separator = false;
            } else {
                separator = true;
            }
        }
        if result.is_empty() {
            fallback.to_owned()
        } else {
            result
        }
    }
    format!(
        "{}-{}",
        slug(workspace, "workspace"),
        slug(session, "session")
    )
}

fn named_command(command: &str, label: &str) -> String {
    let Some(words) = simple_words(command) else {
        return command.to_owned();
    };
    let Some((executable, end)) = words.first() else {
        return command.to_owned();
    };
    // Kind enables the policy, but does not promise that a custom wrapper
    // accepts Claude's flags. Do not rewrite wrappers or compound shell code.
    if std::path::Path::new(executable)
        .file_name()
        .and_then(|s| s.to_str())
        != Some("claude")
    {
        return command.to_owned();
    }
    let has_name = words
        .iter()
        .skip(1)
        .take_while(|(word, _)| word != "--")
        .any(|(word, _)| word == "--name" || word.starts_with("--name=") || word.starts_with("-n"));
    if has_name {
        return command.to_owned();
    }
    // Insert before arguments, so a prompt or -- cannot consume the new flag.
    format!("{} --name {label}{}", &command[..*end], &command[*end..])
}

/// Conservative lexer for a single literal executable plus quoted arguments.
/// Reject expansion/operators instead of attempting to rewrite a shell program.
/// Byte offsets let us insert a flag without reserializing user arguments.
fn simple_words(command: &str) -> Option<Vec<(String, usize)>> {
    let mut chars = command.char_indices().peekable();
    let mut words = Vec::new();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|(_, ch)| matches!(ch, ' ' | '\t')) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut word = String::new();
        let mut quote = None;
        let mut end = 0;
        while let Some((index, ch)) = chars.next() {
            end = index + ch.len_utf8();
            match (quote, ch) {
                (Some('\''), '\'') | (Some('"'), '"') => quote = None,
                (Some('\''), _) => word.push(ch),
                (None, '\'' | '"') => quote = Some(ch),
                (None, ' ' | '\t') => break,
                (_, '\\') => {
                    let (next_index, next) = chars.next()?;
                    if next == '\n' || next == '\r' {
                        return None;
                    }
                    if quote == Some('"') && !matches!(next, '$' | '`' | '"' | '\\') {
                        word.push('\\');
                    }
                    word.push(next);
                    end = next_index + next.len_utf8();
                }
                (_, '$' | '`') => return None,
                (
                    None,
                    ';' | '&' | '|' | '<' | '>' | '(' | ')' | '{' | '}' | '#' | '*' | '?' | '['
                    | ']' | '\n' | '\r',
                ) => return None,
                _ => word.push(ch),
            }
            if quote.is_none() && chars.peek().is_some_and(|(_, ch)| matches!(ch, ' ' | '\t')) {
                break;
            }
        }
        if quote.is_some() {
            return None;
        }
        words.push((word, end));
    }
    Some(words)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn session(command: &str) -> Session {
        Session {
            name: "Session Labels".into(),
            cwd: std::env::temp_dir(),
            command: command.into(),
            kind: Some(SessionKind::ClaudeCode),
            env: BTreeMap::new(),
            description: None,
        }
    }

    #[test]
    fn labels_are_readable_safe_slugs_with_nonempty_fallbacks() {
        assert_eq!(
            session_label("Portagenty", "Session Labels"),
            "portagenty-session-labels"
        );
        assert_eq!(
            session_label("  My / Work_SPACE! ", "Fix #42"),
            "my-work-space-fix-42"
        );
        assert_eq!(session_label("!!!", "日本語"), "workspace-session");
    }

    #[test]
    fn direct_claude_preserves_executable_arguments_and_prompt_separator() {
        for (command, expected) in [
            ("claude", "claude --name demo-label"),
            (
                "  claude\t--continue",
                "  claude --name demo-label\t--continue",
            ),
            (
                "'/opt/My Apps/claude' --model sol",
                "'/opt/My Apps/claude' --name demo-label --model sol",
            ),
            (
                "claude -- 'explain --name'",
                "claude --name demo-label -- 'explain --name'",
            ),
            (
                "claude 'hello; world'",
                "claude --name demo-label 'hello; world'",
            ),
        ] {
            assert_eq!(named_command(command, "demo-label"), expected);
        }
    }

    #[test]
    fn explicit_names_are_preserved() {
        for command in [
            "claude --name 'My chosen name' --continue",
            "claude --name=mine",
            "claude -n mine",
            "claude -nmine",
            "claude '-n' mine",
            "claude \"--name\" mine",
        ] {
            assert_eq!(named_command(command, "ignored"), command);
        }
    }

    #[test]
    fn custom_commands_and_shell_programs_are_not_rewritten() {
        for command in [
            "cc --continue",
            "claude-sol",
            "exec claude",
            "env MODEL=sol claude",
            "claude && echo finished",
            "claude; echo finished",
            "claude | cat",
            "claude > output",
            "claude # comment",
            "claude\necho finished",
            "claude \"$PROMPT\"",
            "claude $(printf prompt)",
            "claude 'unclosed",
            "printf '%s' 'custom command'",
            "",
            "  ",
        ] {
            assert_eq!(named_command(command, "ignored"), command);
        }
    }

    #[test]
    fn generic_kinds_are_completely_unchanged() {
        for kind in [None, Some(SessionKind::Other), Some(SessionKind::Opencode)] {
            let mut original = session("claude");
            original.kind = kind;
            assert_eq!(prepare_session("Demo", &original), original);
        }
    }

    #[test]
    fn preparation_changes_only_command_not_identity_environment_or_source() {
        let mut original = session("claude");
        original
            .env
            .insert("ANTHROPIC_MODEL".into(), "synthetic-route".into());
        let mut expected = original.clone();
        let prepared = prepare_session("Portagenty", &original);
        assert!(prepared
            .command
            .contains("claude --name portagenty-session-labels"));
        expected.command = prepared.command.clone();
        assert_eq!(prepared, expected);
        assert_eq!(original.command, "claude");
    }

    #[cfg(unix)]
    #[test]
    fn workload_strips_all_markers_and_keeps_routing_inherited_by_mux() {
        // Execute a custom synthetic shell program, never a real agent. Compare
        // in the child; neither inherited nor synthetic env values are printed.
        let checks = INHERITED_SESSION_MARKERS
            .iter()
            .map(|name| format!("test \"${{{name}+present}}\" != present"))
            .chain([
                "test \"$ANTHROPIC_MODEL\" = synthetic-route".into(),
                "test \"$HTTPS_PROXY\" = synthetic-proxy".into(),
                "test \"$CLAUDE_CODE_SUBAGENT_MODEL\" = synthetic-worker".into(),
                "test \"$AGENT_GUARD_SUBAGENT_MODEL_LOCK\" = synthetic-lock".into(),
            ])
            .collect::<Vec<_>>()
            .join(" && ");
        let prepared = prepare_session("Demo", &session(&checks));
        let mut child = std::process::Command::new("sh");
        child.arg("-c").arg(&prepared.command);
        for name in INHERITED_SESSION_MARKERS {
            child.env(name, "synthetic-parent-marker");
        }
        let output = child
            .env("ANTHROPIC_MODEL", "synthetic-route")
            .env("HTTPS_PROXY", "synthetic-proxy")
            .env("CLAUDE_CODE_SUBAGENT_MODEL", "synthetic-worker")
            .env("AGENT_GUARD_SUBAGENT_MODEL_LOCK", "synthetic-lock")
            .output()
            .unwrap();
        assert!(output.status.success(), "child environment checks failed");
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn custom_shell_command_retains_quotes_operators_and_exit_status() {
        let command = "value=\"it's literal\"; [[ $value == \"it's literal\" ]] && exit 17";
        let prepared = prepare_session("Demo", &session(command));
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(prepared.command)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(17));
    }

    #[cfg(unix)]
    #[test]
    fn synthetic_claude_receives_name_and_original_args_without_parent_markers() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("My Apps");
        std::fs::create_dir(&directory).unwrap();
        let executable = directory.join("claude");
        std::fs::write(&executable, r#"#!/bin/bash
[[ $# == 4 && $1 == --name && $2 == portagenty-session-labels && $3 == --model && $4 == synthetic ]] || exit 31
[[ -z ${CLAUDECODE+present} && -z ${CLAUDE_CODE_CHILD_SESSION+present} ]] || exit 32
[[ $ANTHROPIC_MODEL == synthetic-route ]] || exit 33
"#).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let original = session(&format!(
            "{} --model synthetic",
            shell_quote(executable.to_str().unwrap())
        ));
        let prepared = prepare_session("Portagenty", &original);
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&prepared.command)
            .env("CLAUDECODE", "synthetic-parent-marker")
            .env("CLAUDE_CODE_CHILD_SESSION", "synthetic-parent-marker")
            .env("ANTHROPIC_MODEL", "synthetic-route")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "synthetic argv/environment check failed"
        );
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn both_mux_transports_keep_prepared_label_and_cleanup_payload() {
        let prepared = prepare_session("Portagenty", &session("claude --continue"));
        let target = crate::mux::workspace_session_name("Portagenty", &prepared.name);
        let args = crate::mux::TmuxAdapter::new()
            .create_detached_args(&prepared, &target)
            .unwrap();
        assert_eq!(args.last().unwrap(), prepared.command.as_str());
        let layout = crate::mux::zellij::render_layout_with_tab_name(
            &prepared,
            "Portagenty / Session Labels",
        );
        assert!(layout.contains("claude --name portagenty-session-labels --continue"));
        for name in INHERITED_SESSION_MARKERS {
            assert!(layout.contains(&format!("-u {name}")));
        }
        assert_eq!(
            target,
            crate::mux::workspace_session_name("Portagenty", "Session Labels")
        );
    }
}
