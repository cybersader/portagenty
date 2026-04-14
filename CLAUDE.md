# CLAUDE.md — portagenty

## What this project is

A portable, terminal-native (TUI) launcher for agent workspaces, written in Rust. Binary name is **`pa`**. Replaces `vscode-terminal-workspaces` for the author's personal use and adds a first-class **workspace** concept — "hierarchy on top of hierarchy" — over the filesystem.

Sources of truth in this repo:

- [README.md](./README.md) — vision, what it is / is not, locked tech decisions.
- [DESIGN.md](./DESIGN.md) — architectural deep-dive; the arbiter for terminology and architectural decisions.
- [ROADMAP.md](./ROADMAP.md) — v1 / v1.x / v2+ sequencing.

If any of those three conflict with this file, update this file to match — not the other way around.

## Current stage

**Pre-code.** The repo contains vision + architecture + roadmap docs only. Do not scaffold Rust code, add `Cargo.toml`, or create `src/` until the user explicitly asks for it.

## Hard constraints (locked)

These were confirmed in the 2026-04 init Q&A and are not up for debate without the user reopening them.

- **Rust.** Chosen for trust, security, and long-term maintenance — not raw speed. Prefer well-maintained crates with strong ecosystems.
- **Single static binary.** Portability is a core requirement. Anything that breaks `scp`-and-run deployment is a red flag (dynamic linking to exotic system libs, required runtimes, config baked into absolute paths, etc.).
- **Terminal-native only.** No GUI, no web UI, no VS Code extension. Ever.
- **Binary name is `pa`.** Not `pg` (PostgreSQL conflict), not `portagenty` as the primary CLI (too long to type).
- **TOML everywhere.** Config format is TOML across all three tiers.
- **Three-tier config.** Global (`$XDG_CONFIG_HOME/portagenty/`) + workspace file + per-project `portagenty.toml`. Any tier can register a project or session. See `DESIGN.md` §2.
- **Workspace files are designed to commit.** No absolute paths, no machine-specific state. Local state goes in a separate split state store.
- **No SQLite in v1.** State is TOML files for durable stuff + live polling of the multiplexer for volatile stuff. Don't reach for a DB preemptively.
- **Agnostic-core agent story.** A session is `name + cwd + command`. Core doesn't know Claude Code from vim. Optional `kind:` hints are a v1.x feature.
- **Workspace-scoped lazy launch.** Entering a workspace does not spawn processes. Sessions are created on first open.
- **tmux is the reference multiplexer for v1 implementation.** Zellij and WezTerm adapters come in v1.x. When in doubt during early implementation work, make it work in tmux first; design the adapter interface so the other two can slot in without core changes.

## Workspace model in one paragraph

Filesystems force every project into one parent directory. portagenty layers *alternative hierarchies* over that fixed hierarchy: Recently Opened (LRU), Tags, Custom Ordered Groups. Projects live wherever they live on disk; portagenty never moves or symlinks anything. A workspace is a first-class file that names a curated view — a query, a tag set, or an ordered list — over real project paths. The same project can appear in many workspaces. Workspaces are findable (walk-up from `$PWD`, global registry, or explicit path) and committable.

## Sibling projects (related but separate)

- `agentic-workflow-and-tech-stack` — the scaffold for agent-ready projects. portagenty launches such projects but doesn't depend on the scaffold's specific layout. Scaffolding itself is out of scope for portagenty.
- `vscode-terminal-workspaces` — the VS Code extension being replaced for the author's personal workflow. Useful reference for which features matter (profiles, nested folders, untracked-session discovery). Don't copy its architecture; copy its good ideas selectively.

## Working style for this repo

- When discussing design, propose 2–3 options with tradeoffs rather than a single answer. The user wants to make decisions, not rubber-stamp them.
- Prefer updating `README.md`, `DESIGN.md`, or `ROADMAP.md` over creating parallel design docs. Those three files are the whole design surface.
- If a user decision conflicts with anything in those docs, update the docs in the same turn. Stale docs here are a worse failure mode than verbose docs.
- Memory notes at `~/.claude/projects/-mnt-c-Users-Cybersader-Documents-1-Projects--Workspaces-portagenty/memory/` hold cross-session context (sibling repos, locked decisions). Don't put design decisions there — those go in `DESIGN.md`.
