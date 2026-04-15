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

Examples:

```sh
pa launch claude
pa launch claude --dry-run
pa launch claude -w ~/code/my.portagenty.toml
pa launch claude --shared            # leave other devices attached
```

## `pa claim [session]`

"Make this device the main session." Short-form alias for
takeover-attach. Session name defaults to the first one declared in
the workspace.

| Flag | Default | What |
|---|---|---|
| `-w`, `--workspace <path>` | walk-up | Explicit workspace file |
| `--dry-run` | off | Print what would happen |

Examples:

```sh
pa claim                  # first session in workspace
pa claim tests            # specific session
pa claim --dry-run        # peek without touching
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
