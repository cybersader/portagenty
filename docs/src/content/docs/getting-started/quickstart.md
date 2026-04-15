---
title: Quickstart
description: Open the TUI and launch your first session.
sidebar:
  order: 2
---

## 1. Define a workspace

Create a `*.portagenty.toml` anywhere — typically at the root of a
directory that holds related projects. The prefix before
`.portagenty.toml` can be anything non-empty (e.g.
`agentic.portagenty.toml`, `work.portagenty.toml`).

```toml
name = "Example workspace"
multiplexer = "tmux"
projects = ["~/code/portagenty"]

[[session]]
name = "claude"
cwd = "~/code/portagenty"
command = "claude"
```

## 2. Open the TUI

```sh
cd /path/under/your/workspace
pa
```

`pa` walks up from `$PWD` looking for a `*.portagenty.toml`. It loads
the workspace, resolves paths (`~`, `${VAR}`), merges in any
per-project `portagenty.toml` files, and renders the session list.
Nothing is launched yet.

## 3. Navigate and launch

| Key | Action |
|-----|--------|
| `j` / `↓` | Next session |
| `k` / `↑` | Previous session |
| `g` / `Home` | First session |
| `G` / `End` | Last session |
| `Enter` | Launch the highlighted session via tmux |
| `q` / `Esc` / `Ctrl-C` | Quit the TUI |

When you press `Enter`, the TUI restores the terminal and hands the
TTY to tmux via `tmux new-session` (or `tmux attach-session` if the
session already exists). Detach with the multiplexer's normal binding
(`Ctrl-b d` for tmux) to return to your shell.

## 4. Scriptable equivalents

```sh
pa list                     # print the resolved workspace to stdout
pa launch claude            # skip the TUI, launch a session by name
pa launch claude --dry-run  # print what would happen without running it
pa launch claude -w ./my.portagenty.toml   # explicit workspace file

# "Make this device the main session." Defaults the session name to the
# first one in the workspace — the common one-agent-per-project case.
pa claim
pa claim tests              # specific session by name

# Render the workspace as a starter script (or zellij layout). Useful
# for committing alongside the TOML so teammates can launch the whole
# stack without installing pa themselves.
pa export                       # to stdout; format follows the workspace's mpx
pa export --format zellij -o layout.kdl
pa export --format tmux -o starter.sh
```

## 5. Cross-device: take over from a different machine

When you SSH in from a phone (or any other device) and attach to a
session that's already attached on your desktop, tmux (and by extension
`pa`) will shrink the session to the smaller client's terminal size and
keep it that way. That's the infamous "stuck in a weird size" bug.

`pa`'s default behavior fixes this for you:

- `pa`, `pa launch <name>`, and `pa claim` all default to **takeover**
  mode. On attach, any other clients get detached so the session
  reshapes to the current device.
- The session itself keeps running — the other device's *client* is
  disconnected, not the session. You can re-attach from the other
  device later; it'll just reconnect to the still-running session.
- Pass `--shared` to opt out:

  ```sh
  pa launch claude --shared    # multiple devices can watch concurrently
  ```

`pa claim` is a shorter verb for the common "I moved devices, make this
one the main" flow — it's the same as `pa launch <first-session>` but
without having to remember the session name.

**Multiplexer caveats:**

- **tmux**: uses `tmux attach -d` internally — takeover works cleanly.
- **zellij**: has no CLI equivalent to force-detach other clients.
  `pa` warns when other clients appear to be attached, but you may
  need to manually detach the other device (Ctrl+Q on the other end)
  if screen-size weirdness persists.

## What gets recorded

Each launch writes an entry to
`$XDG_STATE_HOME/portagenty/state.toml` (or
`~/.local/state/portagenty/state.toml` if unset). It's
machine-local and not committed anywhere. v1 tracks the data; the
**Recent** view that displays it is a v1.x feature.

## Per-project sessions

A project can ship its own `portagenty.toml` at its root:

```toml
# ~/code/portagenty/portagenty.toml
[[session]]
name = "tests"
cwd = "."
command = "cargo nextest run"
```

When a workspace lists a project via `projects = [...]`, portagenty
merges that project's sessions into the workspace view. Name
collisions: the workspace's version wins. See
[the three-tier merge](../concepts/#three-tier-config) for the full
rules.
