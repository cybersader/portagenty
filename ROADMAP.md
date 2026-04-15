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

## v1.x — Follow-ons

### Shipped

1. **zellij adapter** (`af963c3…`). List/has/kill via imperative CLI;
   create + attach via generated KDL layout files. Inside-zellij
   detection returns a clear "detach first" error instead of the
   opaque nesting failure. 7 e2e tests against real zellij on Linux
   CI.
3. **Untracked session adoption.** The TUI merges `mux.list_sessions`
   with workspace definitions and surfaces three row states — Live,
   NotStarted, Untracked — each with a distinct color marker.
   Enter routes to `attach` for Live/Untracked and `create_and_attach`
   for NotStarted.
6. **State/activity decorations.** Delivered alongside untracked
   adoption: ● (green) live, ○ (dim) idle, ? (yellow) untracked,
   plus a `[label]` tag on each row.
9. **`kind:` session hints.** Optional `kind` field on sessions
   (claude-code / opencode / editor / dev-server / shell / other).
   Currently display-only — the TUI shows a one-letter color-coded
   glyph (C / O / E / D) next to the state marker. Smart-resume
   behaviors deferred to a later commit if we find they're worth
   the added branching.

### Still to ship (rough priority order)

2. ~~**WezTerm adapter.**~~ **Deferred / not-the-right-fit.** WezTerm
   has a mux subsystem, but it's built around the GUI terminal
   emulator's own multi-window model — not the headless
   "detach-from-desktop, reattach-from-phone-over-SSH" pattern that
   tmux and zellij are explicitly designed for. portagenty's whole
   value-add over `cd && claude` is the cross-device session-
   persistence story (see DESIGN §10 + the `pa claim` cross-device
   takeover work in v1.x). WezTerm doesn't move the needle there,
   and a half-baked adapter would mislead users into setups that
   silently lose their state on disconnect. The `Multiplexer::Wezterm`
   enum variant stays so workspace files can pin it ahead of any
   future change of mind, but `build_mux` returns a clear "use
   tmux or zellij" message until/unless the upstream model evolves.
4. **Tags view, polished.** Tag editing from the TUI. Tag-based
   filtering and grouping. Thread `tags` from the global registry
   into the resolved `Workspace`.
5. **Custom ordered groups.** Hand-curated named groups
   ("playlists"). Drag-in-TUI or edit via keybinding.
7. **Declarative export.** `pa export` emits zellij KDL layouts
   and tmux scripts from a workspace definition.
8. **Eager / "jump-in" launch.** `--eager` flag and/or
   workspace-level config key.
10. **Session schema extensions.** Env vars, pre/post commands,
    profile references. Lifted selectively from the VS Code
    extension's schema.
11. **`fd` / Everything CLI search integration.** Opportunistic
    fast discovery where available.
12. **Termux polish pass.** Over-SSH verification + any rough
    edges found in real use. Core keybindings are already
    Termux-safe by default (`DESIGN.md` §10).

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
