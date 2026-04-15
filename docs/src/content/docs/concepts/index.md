---
title: Concepts
description: The vocabulary portagenty uses, defined precisely.
---

The full architectural deep-dive lives in
[DESIGN.md](https://github.com/cybersader/portagenty/blob/main/DESIGN.md)
in the repo. This page is the short version — one-line definitions
for everything you'll see in the TUI and the TOML.

## Project

A directory on disk with code or content you work on. Registered
with portagenty at any of three tiers (global, workspace, or
per-project `portagenty.toml`). Identified by its filesystem path.

## Workspace

A named, curated view over one or more projects plus the sessions
you use to work on them. First-class file on disk
(`*.portagenty.toml`), designed to be committable. A workspace is
where "hierarchy on top of hierarchy" happens — same underlying
projects, many possible views (recency, tags, custom groups — the
latter two still on the roadmap).

## Session

One unit of execution: a shell, a process, an agent. Core schema is
`name + cwd + command`, plus optional `kind` (display hint) and
`env` (key-value env vars). A session belongs to a workspace.

## Session state (live / idle / untracked)

Shown in the TUI as colored markers next to each row:

| Marker | State | Source | Enter does |
|---|---|---|---|
| ● (green) | Live | Workspace session, currently running in mpx | `attach` |
| ○ (dim) | Idle | Workspace session, not yet started | `create_and_attach` |
| ? (yellow) | Untracked | Running in mpx, not in workspace TOML | `attach` |

Untracked = the tmux/zellij session you started manually last week
that `pa` can see via `list-sessions` and let you re-attach to.

## Kind hint

Optional `kind:` field on a session: `claude-code`, `opencode`,
`editor`, `dev-server`, `shell`, or `other`. Purely display in v1.x —
the TUI shows a one-letter colored glyph (C / O / E / D) next to the
state marker. Smart-resume (e.g. `claude --continue`) is not wired
to the kind in v1.x; you can express that in `command` directly.

## Multiplexer / adapter

tmux or zellij. The thing that owns the terminal panes and keeps
them alive across detaches. portagenty drives it via its CLI — it
doesn't replace it. Each adapter (`TmuxAdapter`, `ZellijAdapter`) is
a Rust implementation of the `Multiplexer` trait.

## Attach mode (takeover / shared)

Two shapes a `pa attach` can take:

- **Takeover** (default): detach any other clients on attach. Session
  keeps running; the other device's *client* returns to its shell.
  Fixes the "screen size stuck to smaller client" multi-client issue.
- **Shared**: attach without disturbing other clients. Pass `--shared`
  to `pa launch` or use `pa launch <name> --shared`.

`pa claim` is a short verb for takeover-attach; it defaults the
session name to the first one in the workspace.

## Three-tier config merge

Sessions + project registrations can be declared at:

1. **Global** — `$XDG_CONFIG_HOME/portagenty/config.toml`.
   Machine-local, not committed.
2. **Workspace** — any `*.portagenty.toml`. Meant to commit.
3. **Per-project** — `portagenty.toml` at a project root. Meant to
   commit.

Merge rule on session-name collision: **workspace > per-project >
global**. Closer to the user's current intent wins.

## State store

`$XDG_STATE_HOME/portagenty/state.toml`. Records a bounded LRU of
recent launches. Machine-local, not committed. Feeds the picker's
recency sort and the session list's "LAST" column.

## Workspace picker (home screen)

When you run bare `pa` from a directory with no walkable
`*.portagenty.toml`, `pa` shows a **picker**: a ratatui home screen
listing every workspace registered in your global config, sorted by
recency (most-recent on top, never-launched alphabetical below). A
bottom "live sessions on this machine" row gives you an ad-hoc browse
mode — attach to any live tmux/zellij session without authoring TOML.

Auto-registration: `pa init` and the onboarding wizard both append
the new workspace to `[[workspace]]` in the global config, so future
`pa` invocations see it from anywhere.

Navigation follows Android-back semantics:

- **Esc** from the session TUI → always returns to the picker.
- **Esc / q** from the picker → exit `pa`.
- **q / Ctrl+C** from anywhere → exit `pa` directly.

The picker is also a "jump to another workspace" affordance for
walk-up users: enter via walk-up, press Esc once, and you're on the
home screen with every other registered workspace one keypress away.

## Visual differentiation in the TUI

The explorer encodes state with color, not just glyphs:

- **Title bar** shows a colored mpx badge — cyan `[tmux]`, magenta
  `[zellij]` — plus session count and an untracked-count badge in
  yellow when live sessions exist outside your workspace definition.
- **Session rows** color the name itself, not just the marker:
  green for `Live`, dim for `NotStarted`, yellow for `Untracked`.
- **Kind glyphs** get per-kind colors (blue `C` for claude-code,
  cyan `O` for opencode, magenta `E` for editor, green `D` for
  dev-server).
- **Attached-client count** on tmux live rows: `[live · 2 clients]`
  / `[live · 1 client]` / `[live · detached]`. Zellij doesn't expose
  per-session client counts, so those stay `[live]`.
- **Recency** shows twice: the picker lists "X ago" per workspace,
  the session list adds a `LAST` column on live rows at widths
  ≥ 80 cols.

## Narrow-terminal layout

At widths below 60 columns, each session row renders as a two-line
card — marker + name + status on line 1, indented dim `cmd  ·  path`
on line 2 — so the essentials stay readable on a phone keyboard in
portrait. The footer's keybind hints shorten to fit
(`Esc: back · q: quit` at the narrowest). See
[Termux](../../getting-started/termux/) for the full mobile story.

## Pre-launch banner

When you press Enter to attach, `pa` restores your terminal and
prints a one-line banner before the multiplexer takes over:

```
  pa → zellij session "claude"
        detach: Ctrl+O then d  ·  re-attach: pa claim claude
```

Informational only — `pa` never rebinds your mpx keys. The detach
chord shown is the multiplexer's default; if you've customized it,
use your own chord. (Opinionated mpx-config belongs in a dev-
environment scaffold, not in the launcher.)
