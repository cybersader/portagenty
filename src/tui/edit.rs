//! In-TUI session edit overlay (the `e` key in the session list).
//!
//! Two-stage UX:
//! 1. Field chooser — single-keypress to pick which field to edit
//!    (`r` rename / `c` cwd / `m` command / `k` kind / `e` env).
//! 2. Per-field editor — a small text-input prompt for free-form
//!    fields, or a sub-chooser for `kind` (closed enum) and `env`
//!    (set vs unset, then KEY then VAL).
//!
//! On confirm: routes through `crate::cli::edit_session_in_file`
//! (the same toml_edit-preserving helper the CLI uses) so there's
//! exactly one place that mutates the workspace file. Reload the
//! in-memory workspace + rebuild rows after a successful write.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};

/// Stage of the edit overlay. Each stage owns its own state because
/// stages have distinct UX (single keypress vs. text input vs.
/// sub-chooser).
#[derive(Debug, Clone)]
pub enum EditState {
    /// Top-level field picker.
    PickField,
    /// Free-form text editor for one of name / cwd / command.
    TypingValue { field: TextField, input: String },
    /// Closed-enum chooser for `kind`.
    PickingKind,
    /// Sub-chooser for env: set or unset?
    EnvAction,
    /// Typing a KEY for env-set / env-unset.
    EnvKey { action: EnvAction, input: String },
    /// Typing a VAL for env-set after the key was provided.
    EnvVal { key: String, input: String },
}

/// Free-text fields that just need a single line of input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextField {
    Rename,
    Cwd,
    Command,
}

impl TextField {
    pub fn label(self) -> &'static str {
        match self {
            TextField::Rename => "rename to",
            TextField::Cwd => "cwd",
            TextField::Command => "command",
        }
    }
}

/// Set vs. unset for env operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvAction {
    Set,
    Unset,
}

/// What the outer App should do after a key press.
#[derive(Debug, Clone)]
pub enum EditOutcome {
    /// No state change visible to the caller; keep rendering.
    Continue,
    /// User cancelled — close the overlay.
    Cancel,
    /// User confirmed the edit. Caller should call
    /// `crate::cli::edit_session_in_file` with this op and reload.
    Apply(crate::cli::EditOp),
    /// User chose cwd edit — caller should open the find/tree
    /// overlay for folder selection instead of a text input.
    BrowseForCwd,
}

/// Process a single key press inside the edit overlay. Pure
/// dispatch — caller redraws.
pub fn handle_key(state: &mut EditState, code: KeyCode, mods: KeyModifiers) -> EditOutcome {
    // Esc always cancels at any stage.
    if matches!(code, KeyCode::Esc) {
        return EditOutcome::Cancel;
    }
    // Ctrl+C cancels too.
    if matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL) {
        return EditOutcome::Cancel;
    }
    match state {
        EditState::PickField => match code {
            KeyCode::Char('r') => {
                *state = EditState::TypingValue {
                    field: TextField::Rename,
                    input: String::new(),
                };
                EditOutcome::Continue
            }
            KeyCode::Char('c') => EditOutcome::BrowseForCwd,
            KeyCode::Char('m') => {
                *state = EditState::TypingValue {
                    field: TextField::Command,
                    input: String::new(),
                };
                EditOutcome::Continue
            }
            KeyCode::Char('k') => {
                *state = EditState::PickingKind;
                EditOutcome::Continue
            }
            KeyCode::Char('e') => {
                *state = EditState::EnvAction;
                EditOutcome::Continue
            }
            _ => EditOutcome::Continue,
        },
        EditState::TypingValue { field, input } => match code {
            KeyCode::Backspace => {
                input.pop();
                EditOutcome::Continue
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                input.clear();
                EditOutcome::Continue
            }
            KeyCode::Char(ch) => {
                input.push(ch);
                EditOutcome::Continue
            }
            KeyCode::Enter => {
                if input.is_empty() {
                    return EditOutcome::Continue;
                }
                let value = input.clone();
                let op = match field {
                    TextField::Rename => crate::cli::EditOp {
                        rename: Some(value),
                        ..Default::default()
                    },
                    TextField::Cwd => crate::cli::EditOp {
                        cwd: Some(value),
                        ..Default::default()
                    },
                    TextField::Command => crate::cli::EditOp {
                        command: Some(value),
                        ..Default::default()
                    },
                };
                EditOutcome::Apply(op)
            }
            _ => EditOutcome::Continue,
        },
        EditState::PickingKind => {
            use crate::domain::SessionKind;
            let k = match code {
                KeyCode::Char('1') | KeyCode::Char('c') => Some(SessionKind::ClaudeCode),
                KeyCode::Char('2') | KeyCode::Char('o') => Some(SessionKind::Opencode),
                KeyCode::Char('3') | KeyCode::Char('e') => Some(SessionKind::Editor),
                KeyCode::Char('4') | KeyCode::Char('d') => Some(SessionKind::DevServer),
                KeyCode::Char('5') | KeyCode::Char('s') => Some(SessionKind::Shell),
                KeyCode::Char('6') | KeyCode::Char('x') => Some(SessionKind::Other),
                _ => None,
            };
            match k {
                Some(kind) => {
                    let op = crate::cli::EditOp {
                        kind: Some(kind),
                        ..Default::default()
                    };
                    EditOutcome::Apply(op)
                }
                None => EditOutcome::Continue,
            }
        }
        EditState::EnvAction => match code {
            KeyCode::Char('s') | KeyCode::Char('1') => {
                *state = EditState::EnvKey {
                    action: EnvAction::Set,
                    input: String::new(),
                };
                EditOutcome::Continue
            }
            KeyCode::Char('u') | KeyCode::Char('2') => {
                *state = EditState::EnvKey {
                    action: EnvAction::Unset,
                    input: String::new(),
                };
                EditOutcome::Continue
            }
            _ => EditOutcome::Continue,
        },
        EditState::EnvKey { action, input } => match code {
            KeyCode::Backspace => {
                input.pop();
                EditOutcome::Continue
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                input.clear();
                EditOutcome::Continue
            }
            KeyCode::Char(ch) => {
                input.push(ch);
                EditOutcome::Continue
            }
            KeyCode::Enter => {
                if input.is_empty() {
                    return EditOutcome::Continue;
                }
                match action {
                    EnvAction::Unset => {
                        let op = crate::cli::EditOp {
                            env_unset: vec![input.clone()],
                            ..Default::default()
                        };
                        EditOutcome::Apply(op)
                    }
                    EnvAction::Set => {
                        // Move on to value entry.
                        let key = input.clone();
                        *state = EditState::EnvVal {
                            key,
                            input: String::new(),
                        };
                        EditOutcome::Continue
                    }
                }
            }
            _ => EditOutcome::Continue,
        },
        EditState::EnvVal { key, input } => match code {
            KeyCode::Backspace => {
                input.pop();
                EditOutcome::Continue
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                input.clear();
                EditOutcome::Continue
            }
            KeyCode::Char(ch) => {
                input.push(ch);
                EditOutcome::Continue
            }
            KeyCode::Enter => {
                let op = crate::cli::EditOp {
                    env_set: vec![(key.clone(), input.clone())],
                    ..Default::default()
                };
                EditOutcome::Apply(op)
            }
            _ => EditOutcome::Continue,
        },
    }
}

/// Render the overlay over `area`. Centered, hugs content width.
pub fn render(frame: &mut Frame<'_>, area: Rect, session_name: &str, state: &EditState) {
    let (title, body) = body_for(session_name, state);

    let w = area.width;
    let h = area.height;
    let max_line = body
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0)
        .max(title.len() + 4);
    let overlay_w = ((max_line as u16).saturating_add(4))
        .min(w.saturating_sub(2))
        .max(28)
        .min(w);
    let want_h = (body.len() as u16).saturating_add(2);
    let overlay_h = want_h.min(h.saturating_sub(2)).max(6).min(h);

    let x = area.x + (w.saturating_sub(overlay_w)) / 2;
    let y = area.y + (h.saturating_sub(overlay_h)) / 2;
    let region = Rect {
        x,
        y,
        width: overlay_w,
        height: overlay_h,
    };
    frame.render_widget(Clear, region);

    let block = Block::default()
        .title(format!(" {title} "))
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(Paragraph::new(body).block(block), region);
}

fn body_for(session_name: &str, state: &EditState) -> (String, Vec<Line<'static>>) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let cyan = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let prompt = |label: &str, input: &str| {
        Line::from(vec![
            Span::raw("  "),
            Span::styled("❯ ", cyan),
            Span::styled(format!("{label}: "), dim),
            Span::styled(input.to_string(), bold),
            Span::styled("_", cyan),
        ])
    };
    match state {
        EditState::PickField => (
            format!("edit session: {session_name}"),
            vec![
                Line::raw(""),
                Line::from(vec![Span::raw("  pick a field to edit:")]),
                Line::raw(""),
                row_key("r", "rename"),
                row_key("c", "cwd"),
                row_key("m", "command"),
                row_key("k", "kind"),
                row_key("e", "env (set / unset)"),
                Line::raw(""),
                Line::from(vec![Span::styled("  Esc cancels at any stage.", dim)]),
            ],
        ),
        EditState::TypingValue { field, input } => (
            format!("edit {} of {}", field.label(), session_name),
            vec![
                Line::raw(""),
                prompt(field.label(), input),
                Line::raw(""),
                Line::from(vec![Span::styled(
                    "  Enter to apply  ·  Backspace deletes  ·  Ctrl+U clears  ·  Esc cancels",
                    dim,
                )]),
            ],
        ),
        EditState::PickingKind => (
            format!("edit kind of {session_name}"),
            vec![
                Line::raw(""),
                Line::from(vec![Span::raw("  pick a kind:")]),
                Line::raw(""),
                row_key("1", "claude-code  (or c)"),
                row_key("2", "opencode     (or o)"),
                row_key("3", "editor       (or e)"),
                row_key("4", "dev-server   (or d)"),
                row_key("5", "shell        (or s)"),
                row_key("6", "other        (or x)"),
            ],
        ),
        EditState::EnvAction => (
            format!("edit env of {session_name}"),
            vec![
                Line::raw(""),
                Line::from(vec![Span::raw("  set or unset?")]),
                Line::raw(""),
                row_key("s", "set a KEY=VAL  (or 1)"),
                row_key("u", "unset a KEY    (or 2)"),
            ],
        ),
        EditState::EnvKey { action, input } => {
            let title = match action {
                EnvAction::Set => format!("env set on {session_name} — KEY"),
                EnvAction::Unset => format!("env unset on {session_name} — KEY"),
            };
            (
                title,
                vec![
                    Line::raw(""),
                    prompt("KEY", input),
                    Line::raw(""),
                    Line::from(vec![Span::styled(
                        "  Enter to continue  ·  Esc cancels",
                        dim,
                    )]),
                ],
            )
        }
        EditState::EnvVal { key, input } => (
            format!("env set on {session_name} — VAL for {key}"),
            vec![
                Line::raw(""),
                prompt(&format!("VAL ({key}=)"), input),
                Line::raw(""),
                Line::from(vec![Span::styled("  Enter to apply  ·  Esc cancels", dim)]),
            ],
        ),
    }
}

fn row_key(key: &'static str, label: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{key:<3}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::raw(label),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_field_routes_to_correct_typing_state() {
        let mut s = EditState::PickField;
        let _ = handle_key(&mut s, KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(matches!(
            s,
            EditState::TypingValue {
                field: TextField::Rename,
                ..
            }
        ));

        let mut s = EditState::PickField;
        let _ = handle_key(&mut s, KeyCode::Char('m'), KeyModifiers::NONE);
        assert!(matches!(
            s,
            EditState::TypingValue {
                field: TextField::Command,
                ..
            }
        ));
    }

    #[test]
    fn typing_value_appends_chars_and_enter_applies() {
        let mut s = EditState::TypingValue {
            field: TextField::Cwd,
            input: String::new(),
        };
        for ch in ['/', 't', 'm', 'p'] {
            let _ = handle_key(&mut s, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        if let EditState::TypingValue { input, .. } = &s {
            assert_eq!(input, "/tmp");
        } else {
            panic!("wrong state: {s:?}");
        }
        let out = handle_key(&mut s, KeyCode::Enter, KeyModifiers::NONE);
        if let EditOutcome::Apply(op) = out {
            assert_eq!(op.cwd.as_deref(), Some("/tmp"));
        } else {
            panic!("wrong outcome: {out:?}");
        }
    }

    #[test]
    fn esc_cancels_from_any_stage() {
        for mut s in [
            EditState::PickField,
            EditState::TypingValue {
                field: TextField::Rename,
                input: "halfway".into(),
            },
            EditState::PickingKind,
            EditState::EnvAction,
        ] {
            let out = handle_key(&mut s, KeyCode::Esc, KeyModifiers::NONE);
            assert!(matches!(out, EditOutcome::Cancel));
        }
    }

    #[test]
    fn pick_kind_resolves_via_letter_or_number() {
        use crate::domain::SessionKind;
        let mut s = EditState::PickingKind;
        let out = handle_key(&mut s, KeyCode::Char('1'), KeyModifiers::NONE);
        if let EditOutcome::Apply(op) = out {
            assert_eq!(op.kind, Some(SessionKind::ClaudeCode));
        } else {
            panic!()
        }

        let mut s = EditState::PickingKind;
        let out = handle_key(&mut s, KeyCode::Char('s'), KeyModifiers::NONE);
        if let EditOutcome::Apply(op) = out {
            assert_eq!(op.kind, Some(SessionKind::Shell));
        } else {
            panic!()
        }
    }

    #[test]
    fn env_unset_flow_collects_key_and_applies() {
        let mut s = EditState::EnvAction;
        let _ = handle_key(&mut s, KeyCode::Char('u'), KeyModifiers::NONE);
        assert!(matches!(
            s,
            EditState::EnvKey {
                action: EnvAction::Unset,
                ..
            }
        ));
        for ch in ['F', 'O', 'O'] {
            let _ = handle_key(&mut s, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        let out = handle_key(&mut s, KeyCode::Enter, KeyModifiers::NONE);
        if let EditOutcome::Apply(op) = out {
            assert_eq!(op.env_unset, vec!["FOO".to_string()]);
        } else {
            panic!()
        }
    }

    #[test]
    fn env_set_flow_collects_key_then_val() {
        let mut s = EditState::EnvAction;
        let _ = handle_key(&mut s, KeyCode::Char('s'), KeyModifiers::NONE);
        // KEY stage
        for ch in ['F', 'O', 'O'] {
            let _ = handle_key(&mut s, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        let _ = handle_key(&mut s, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(s, EditState::EnvVal { .. }));
        // VAL stage
        for ch in ['b', 'a', 'r'] {
            let _ = handle_key(&mut s, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        let out = handle_key(&mut s, KeyCode::Enter, KeyModifiers::NONE);
        if let EditOutcome::Apply(op) = out {
            assert_eq!(op.env_set, vec![("FOO".into(), "bar".into())]);
        } else {
            panic!()
        }
    }
}
