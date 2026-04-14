# portagenty — ROADMAP

A sequence, not a promise. Each tier is roughly prioritized top-to-bottom; expect reshuffling as reality intrudes.

Companion to [README.md](./README.md) and [DESIGN.md](./DESIGN.md).

---

## v1 — First code milestone

**Definition of done**: the author can replace `cd $dir && cc` with `pa` for their own daily workflow. Everything else is gravy.

Scope:

- Cargo project set up. Dependencies chosen and locked.
- TOML loader for all three config tiers (global, workspace, per-project). Merge rules per `DESIGN.md` §2.
- Workspace file discovery: walk-up from `$PWD`, global registry, explicit `pa open <path>`.
- Session schema v1: `name + cwd + command` only.
- **tmux adapter only.** No zellij, no WezTerm yet. tmux is the stable baseline; getting one adapter right beats three adapters half-right.
- TUI: workspace → sessions tree view. Three view modes over the project list: Recently Opened, Tags, Custom Groups (tags and groups can be empty; LRU is the workhorse early on).
- Workspace-scoped lazy launch: session process is created on first open, not on workspace entry.
- Split state store: durable config in TOML files, volatile live state polled from tmux on refresh.
- Binary name `pa`. Two invocation modes: `pa` (TUI) and `pa launch <workspace>/<session>` (one-shot).
- Platform targets for v1 binary: Linux + macOS. Windows native waits for WezTerm adapter (v1.x).

Explicitly **not** in v1:

- Untracked session adoption. Surface comes after zellij/WezTerm adapters land; adoption UX is cleaner once the adapter interface is stable across all three.
- Env vars / pre/post commands / profile references on sessions.
- `kind:` hints and any agent-aware behavior.
- Declarative export (`pa export`).
- Eager launch flag.
- `fd` / Everything CLI integration for workspace discovery.

---

## v1.x — Follow-ons, in rough order

1. **zellij adapter.** Second mpx. Proves the adapter abstraction holds.
2. **WezTerm adapter.** Unlocks Windows native. Document attach limitations honestly.
3. **Untracked session adoption.** The feature carried over from `vscode-terminal-workspaces`. Ship once all three adapters land, so the UX is consistent.
4. **Tags view, polished.** Tag editing from the TUI. Tag-based filtering and grouping.
5. **Custom ordered groups.** Hand-curated named groups ("playlists"). Drag-in-TUI or edit via keybinding.
6. **State/activity decorations.** Visual indicators (live session, has uncommitted git changes, etc.). Purely decorative overlay on existing views, per decision — *not* a new view.
7. **Declarative export.** `pa export` emits zellij KDL layouts and tmux scripts from a workspace definition.
8. **Eager / "jump-in" launch.** `--eager` flag and/or workspace-level config key.
9. **`kind:` session hints.** Small integrations: "agent running" indicator, smart resume for Claude Code (`--continue`) and OpenCode.
10. **Session schema extensions.** Env vars, pre/post commands, profile references. Lifted selectively from the VS Code extension's schema.
11. **`fd` / Everything CLI search integration.** Opportunistic fast discovery where available.
12. **Termux polish pass.** Measure the TUI over SSH-from-Termux, fix anything that assumes a desktop keyboard. Core keybindings are already Termux-safe by default (`DESIGN.md` §10); this is verification + any rough edges found in real use.

---

## v2+ — Parked

These are plausibly valuable, but we're committing to not thinking about them until v1.x is settled.

- **Agent adapter plugin runtime.** A formal extension mechanism so third parties can add `kind:` handlers without patching the core.
- **Remote-machine awareness.** portagenty as a multi-host tool. Currently: SSH in and run `pa` there.
- **Scaffolding hooks.** A `pa new` subcommand that shells out to a purpose-built project scaffolder.
- **Claude-sessions cross-environment sync.** Either (a) a `pa` plugin that consumes a sync tool, or (b) a documented recommendation pointing at the best-in-class standalone tool. Not owned by the core, but worth a slot here so it doesn't fall off the radar. See `DESIGN.md` §11.
- **Session override layer.** A clean way to commit a workspace file while keeping machine-specific path overrides local.
- **Session dependencies / DAG.** "Start B only after A is attached." Interesting, but risks scope explosion.
- **Multi-workspace views.** A meta-view that spans workspaces (e.g. "all my claude sessions across workspaces").

---

## Non-goals, period

These are not parked; they are explicitly rejected.

- GUI or web UI.
- Embedded agent APIs / wrapping LLM calls.
- An embedded database.
- A daemon / always-running supervisor process.
- Becoming a scaffolder or project-creation tool.
