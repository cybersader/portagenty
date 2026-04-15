---
title: Schema reference
description: Every field you can set in a portagenty TOML file.
sidebar:
  order: 2
---

Three TOML file types, all kebab-case on the wire. See
[DESIGN.md §2](https://github.com/cybersader/portagenty/blob/main/DESIGN.md)
for the full three-tier merge semantics.

## Workspace file (`*.portagenty.toml`)

The prefix before `.portagenty.toml` is required — `portagenty.toml`
by itself is the per-project file, not a workspace file.

```toml
# Required
name = "Agentic stuff"

# Optional — overrides the global default multiplexer
multiplexer = "tmux"                          # or "zellij"

# Optional — projects this workspace covers. Paths accept ~ and ${VAR}.
projects = ["~/code/portagenty", "./cyberbase"]

# Optional — session list. Name collisions resolve workspace-wins.
[[session]]
name = "claude"                               # required
cwd = "~/code/portagenty"                     # required, supports ~, ${VAR}, relative
command = "claude"                            # required
kind = "claude-code"                          # optional; see below
env = { ANTHROPIC_LOG = "debug" }             # optional; per-session env vars

[[session]]
name = "tests"
cwd = "."
command = "cargo nextest run"
```

### `name`
Display name. Also the sanitized base for the mpx session name
(`[^a-zA-Z0-9_-]` → `_`, clamp 50 chars).

### `multiplexer`
`"tmux"` or `"zellij"`. `"wezterm"` parses but fails at launch time
with a clear "use tmux or zellij" message; see
[roadmap rationale](https://github.com/cybersader/portagenty/blob/main/ROADMAP.md).

### `projects`
Paths to project roots. Resolved against the workspace file's
directory for relative entries; `~` expands to `$HOME`; `${VAR}`
expands to the env var.

### `[[session]]`
Array-of-tables. Order is preserved and drives the default TUI
selection order + `pa claim`'s default session.

- **`name`** (required) — unique within the workspace.
- **`cwd`** (required) — absolute, `~`-prefixed, `${VAR}`-templated,
  or relative to the workspace file's directory.
- **`command`** (required) — the shell command. Runs as-is under a
  shell, so pipes/redirections work.
- **`kind`** (optional) — one of `claude-code`, `opencode`,
  `editor`, `dev-server`, `shell`, `other`. Drives the TUI's per-row
  glyph (display-only in v1.x).
- **`env`** (optional) — string-to-string map. Applied via `tmux
  -e KEY=VAL` (tmux) or `env KEY=VAL ... bash -c "<cmd>"` in the
  generated layout (zellij). Iteration order is alphabetical for
  deterministic diffs.

## Per-project file (`portagenty.toml` at a project root)

A lighter-weight file a project can ship to advertise its own
sessions. Merged into any workspace that references the project via
`projects = [...]`.

```toml
[[session]]
name = "tests"
cwd = "."
command = "cargo nextest run"
# kind + env work here too, same as workspace sessions.
```

No top-level fields yet — the project's identity is implicit from
the file's location. Per-project sessions lose to workspace-level
sessions on name collision.

## Global config (`$XDG_CONFIG_HOME/portagenty/config.toml`)

Machine-local. Not committed. Points at workspaces you open often
and sets your default multiplexer.

```toml
default-multiplexer = "tmux"

[[project]]                                    # optional
path = "~/code/portagenty"
tags = ["rust", "agentic"]

[[workspace]]                                  # optional
path = "~/workspaces/agentic.portagenty.toml"
```

### `default-multiplexer`
`"tmux"` or `"zellij"`. Fallback when a workspace file doesn't pin
one. Defaults to `"tmux"` if unset.

### `[[project]]`
Global project registry (optional). `tags` is recognized by the
loader but the filter view that uses it is still on the roadmap.

### `[[workspace]]`
Known workspace files — populates (eventually) a TUI home screen
selector.

## State file (`$XDG_STATE_HOME/portagenty/state.toml`)

Written by `pa` on every launch. Machine-local, not committed. Not
usually something you edit by hand; schema documented here for
completeness.

```toml
[[recent]]
workspace-file = "/home/u/code/my.portagenty.toml"
session-name = "claude"
launched-at-unix = 1700000000
```

Bounded to 50 entries (most-recent first). Dedupes on (workspace,
session) so the same session hitting the top twice doesn't stack.
