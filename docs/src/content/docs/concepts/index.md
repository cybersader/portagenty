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
recent launches. Machine-local, not committed. Used in v1.x by the
"recent" sort (still on the roadmap); v1 just writes to it so the
data exists when the view ships.
