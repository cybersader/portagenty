---
title: Commands reference
description: Every subcommand pa ships, with flags and examples.
sidebar:
  order: 1
---

All commands respect the walk-up workspace discovery — run any of
them from anywhere under your workspace file and `pa` will find it.
The `-w`/`--workspace` flag overrides the walk-up with an explicit
path.

## `pa`

Opens the TUI. No flags today beyond whatever clap adds.

```sh
pa
```

Key bindings inside the TUI:

| Key | Action |
|---|---|
| `j` / `↓` | Next session |
| `k` / `↑` | Previous session |
| `g` / `Home` | First session |
| `G` / `End` | Last session |
| `Enter` | Attach-or-create the highlighted session (takeover) |
| `q` / `Esc` / `Ctrl+C` | Quit |

## `pa launch <session>`

Attach to (or create-and-attach) a specific session by name, without
entering the TUI.

| Flag | Default | What |
|---|---|---|
| `-w`, `--workspace <path>` | walk-up | Explicit workspace file |
| `--dry-run` | off | Print what would happen, don't run it |
| `--shared` | off | Don't detach other clients (see [attach modes](../../concepts/#attach-mode-takeover--shared)) |
| `--resume` | off | Kind-aware resume. For `kind = "claude-code"` sessions, appends `--continue` before launch so Claude picks up its prior conversation. Other kinds print a one-line hint to stderr and launch unchanged. The workspace TOML command string is never mutated on disk. |

Examples:

```sh
pa launch claude
pa launch claude --dry-run
pa launch claude -w ~/code/my.portagenty.toml
pa launch claude --shared            # leave other devices attached
pa launch claude --resume            # claude-code → appends --continue
```

## `pa claim [session]`

"Make this device the main session." Short-form alias for
takeover-attach. Session name defaults to the first one declared in
the workspace.

| Flag | Default | What |
|---|---|---|
| `-w`, `--workspace <path>` | walk-up | Explicit workspace file |
| `--dry-run` | off | Print what would happen |
| `--resume` | off | Same semantics as `pa launch --resume`: appends `--continue` for `kind = "claude-code"` sessions, one-line hint for other kinds. |

Examples:

```sh
pa claim                  # first session in workspace
pa claim tests            # specific session
pa claim --dry-run        # peek without touching
pa claim claude --resume  # takeover + resume the Claude conversation
```

## `pa list`

Print the resolved workspace (name, multiplexer, projects,
sessions) to stdout. Handy for scripts + sanity checks.

```sh
pa list
pa list -w ~/code/my.portagenty.toml
```

Example output:

```
workspace: My stuff
file:      /home/u/code/my.portagenty.toml
mpx:       Tmux
projects:  2
  - /home/u/code/one
  - /home/u/code/two
sessions:  2
  - claude  (cwd: /home/u/code/one)  claude
  - dev     (cwd: /home/u/code/two)  bun run dev
```

## `pa init [name]`

Scaffold a new `<name>.portagenty.toml` in the current directory
with one starter session (`shell` → `bash`). Designed for phone-over-
SSH: you don't have to hand-edit TOML before `pa` works.

| Flag | Default | What |
|---|---|---|
| `name` (positional) | current-directory name | Workspace display name; filename stem is a sanitized version |
| `--mpx tmux\|zellij` | tmux | Which multiplexer to pin |
| `--force` | off | Overwrite an existing `<name>.portagenty.toml` |

```sh
pa init                        # name taken from current dir
pa init my-space               # explicit name
pa init my-space --mpx zellij
pa init my-space --force       # overwrite existing
```

## `pa add <session> -c <command>`

Append a new session to the current workspace file. Faster than
editing TOML manually, especially from a phone keyboard.

| Flag | Default | What |
|---|---|---|
| `name` (positional) | — (required) | New session's name |
| `-c`, `--command <cmd>` | — (required) | Command to run |
| `--cwd <path>` | `.` | Working directory |
| `--kind <...>` | none | `claude-code`, `opencode`, `editor`, `dev-server`, `shell`, or `other` |
| `-w`, `--workspace <path>` | walk-up | Explicit workspace file |

```sh
pa add claude -c "claude --resume" --kind claude-code
pa add dev -c "bun run dev" --cwd ./app --kind dev-server
pa add tests -c "cargo nextest run"
```

The append preserves any comments / formatting in the existing
workspace file — we just tack on a new `[[session]]` block at the
end. Duplicate names error cleanly.

## `pa rm <session>`

Delete a session from the current workspace file. Comments and
formatting elsewhere in the file are preserved — only the matching
`[[session]]` block is excised.

| Flag | Default | What |
|---|---|---|
| `name` (positional) | — (required) | Session to remove |
| `-w`, `--workspace <path>` | walk-up | Explicit workspace file |

```sh
pa rm claude
pa rm tests -w ~/code/my.portagenty.toml
```

## `pa edit <session>`

Change one field on an existing session without opening an editor.
Pass exactly one change flag; passing zero or more than one errors
with guidance.

| Flag | What |
|---|---|
| `name` (positional) | Session to edit |
| `--command <cmd>` | Replace the command |
| `--cwd <path>` | Replace the cwd |
| `--kind <...>` | Replace the kind hint |
| `--rename <new-name>` | Rename (errors on collision with an existing session) |
| `-w`, `--workspace <path>` | Explicit workspace file (walk-up otherwise) |

```sh
pa edit claude --command "claude --resume"
pa edit dev --cwd ./new-app
pa edit my-session --kind claude-code
pa edit old-name --rename new-name
```

Same comment-preserving behavior as `pa rm`: only the target field
on the target session changes; everything else in the file is left
untouched.

## `pa completions <shell>`

Emit a shell completion script to stdout. See
[shell completion setup](../../getting-started/completions/)
for per-shell install recipes.

```sh
pa completions bash > ~/.local/share/bash-completion/completions/pa
pa completions zsh  > ~/.zsh/completions/_pa
pa completions fish > ~/.config/fish/completions/pa.fish
```

Covers subcommand names + flag names + flag values that come from a
closed set. Dynamic completion of session names / snippet names /
workspace files is roadmapped, not v1.x.

## `pa export`

Render the resolved workspace as a multiplexer-native starter
artifact. Useful for committing alongside the workspace TOML so
teammates can launch the whole stack without installing `pa`
themselves.

| Flag | Default | What |
|---|---|---|
| `-w`, `--workspace <path>` | walk-up | Explicit workspace file |
| `--format tmux\|zellij` | workspace's `multiplexer` | Output format |
| `-o`, `--output <path>` | stdout | File to write to |

Examples:

```sh
pa export                             # stdout, format auto-picked
pa export --format zellij             # zellij KDL layout
pa export --format tmux -o starter.sh # save to file
```

Outputs a POSIX shell script for `--format tmux` (`tmux
new-session -d` per session + `tmux attach-session -d` to the
first) or a KDL layout with one tab per session for `--format
zellij`. Both respect env vars declared on sessions and sanitize
session names the same way `pa` does at runtime.

## `pa onboard`

Re-run the first-run wizard at any time. Interactive; walks you
through workspace scaffolding, multiplexer choice (with installed /
not-found annotations), optional Claude Code starter session, and
offers to set or change the machine-default multiplexer. Writes a
`<name>.portagenty.toml` in the current directory and auto-registers
it in the global workspace index so `pa` from anywhere can find it.

```sh
pa onboard
```

No flags — the wizard is fully interactive. Safe to re-run: an
existing workspace file in the current directory is left untouched.

## `pa snippets`

Bundled bash ergonomics shipped inside the `pa` binary. Idempotent:
installing twice replaces the block in-place via a marker comment so
your rc file never accumulates duplicates.

### `pa snippets list`

Print the bundled snippet catalog with one-line descriptions.

```sh
pa snippets list
```

### `pa snippets show <name>`

Print a snippet's contents to stdout. Review before installing.

```sh
pa snippets show pa-aliases
```

### `pa snippets install <name>`

Install (or update) a snippet in your rc file.

| Flag | Default | What |
|---|---|---|
| `name` (positional) | — (required) | Snippet name from `pa snippets list` |
| `--to <path>` | `~/.bashrc` | Target rc file |
| `--dry-run` | off | Preview the result without writing |

```sh
pa snippets install pa-aliases
pa snippets install termux-friendly --to ~/.zshrc
pa snippets install pa-aliases --dry-run
```

### `pa snippets uninstall <name>`

Remove a previously-installed snippet from your rc file. Surrounding
user content is preserved byte-for-byte.

| Flag | Default | What |
|---|---|---|
| `name` (positional) | — (required) | Snippet name to remove |
| `--from <path>` | `~/.bashrc` | Target rc file |
| `--dry-run` | off | Preview the result without writing |

```sh
pa snippets uninstall pa-aliases
```
