<div align="center">

# portagenty

**Portable, terminal-native launcher for agent workspaces.**

Define sessions in TOML. Launch over tmux or zellij.
Hop between devices with `pa claim`. Single static Rust binary.

[![CI](https://github.com/cybersader/portagenty/actions/workflows/ci.yml/badge.svg)](https://github.com/cybersader/portagenty/actions/workflows/ci.yml)

[Install](#install) · [Quickstart](#quickstart) · [Docs](https://cybersader.github.io/portagenty/) · [Design](./DESIGN.md) · [Roadmap](./ROADMAP.md)

</div>

---

> **v1.x feature-complete and usable daily.** See [what shipped](#whats-shipped) below and [ROADMAP.md](./ROADMAP.md) for what's next.

## The problem

```
  Filesystem: one parent per project
  ──────────────────────────────────────────────

  ~/code/              ~/work/             ~/learn/
    portagenty           client-app          rust-book
    cyberbase            api-server          ml-sandbox
    crosswalker

  Want "all my agentic projects"?
  You cd between three different folders.
```

portagenty layers **alternative hierarchies** over that fixed tree — recency, tags, curated groups — without moving anything on disk. Projects stay where they are. A **workspace** is a committable TOML file that names a view over real paths. The same project can appear in many workspaces.

```
  Workspaces: many views, nothing moves
  ──────────────────────────────────────────────

  agentic.portagenty.toml        client.portagenty.toml
    ~/code/portagenty              ~/work/client-app
    ~/work/api-server              ~/work/api-server     ← same project,
    ~/learn/ml-sandbox             ~/code/crosswalker       different context
```

The filesystem is storage. portagenty is the index and launcher on top.

## Install

```sh
cargo install --git https://github.com/cybersader/portagenty
```

No Rust yet?

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
cargo install --git https://github.com/cybersader/portagenty
```

Also install a multiplexer:

```sh
sudo apt install tmux          # Debian / Ubuntu / WSL
brew install tmux              # macOS
# or zellij: https://zellij.dev/documentation/installation
```

Verify: `pa --version`

## Quickstart

```sh
# 1. Drop a workspace file anywhere under the projects it covers.
cat > ~/code/my.portagenty.toml <<'EOF'
name = "My stuff"
multiplexer = "tmux"

[[session]]
name = "claude"
cwd = "~/code/one"
command = "claude"
kind = "claude-code"

[[session]]
name = "dev"
cwd = "~/code/two"
command = "bun run dev"
kind = "dev-server"
EOF

# 2. From any directory under the workspace file:
pa                       # TUI — pick a session, Enter to launch
pa launch claude         # one-shot: skip TUI, go straight to the mpx
pa claim                 # cross-device takeover
pa export -o starter.sh  # render a starter script to commit
```

Or skip the TOML entirely:

```sh
pa init                   # scaffold a workspace in the current dir
pa add claude -c claude --kind claude-code
pa                        # open the TUI
```

## What's shipped

| Category | Features |
|---|---|
| **Multiplexers** | tmux + zellij adapters. WezTerm intentionally deferred. |
| **TUI** | Workspace picker home screen, session list, recency sort, color-coded state markers, kind glyphs, attached-client count, responsive 2-line footer, help overlay (`?`). |
| **Navigation** | Android-back semantics (Esc = back one level). Arrow keys, vim keys (`j`/`k`), Alt+J/K. |
| **Session management** | `pa init` / `add` / `rm` / `edit` from CLI. In-TUI editing (`e` key) for name, cwd, command, kind, env. |
| **Find + scaffold** | `n` in picker: fuzzy-search folders (recency, zoxide, plocate, fd, stdlib walker, nucleo ranking). `Ctrl+R` toggles global search. `Ctrl+T` for tree browser. Scaffold on confirm. |
| **File tree in TUI** | `t` in session list opens a tree rooted at the workspace dir. `.` drills, Backspace pops up, `n` creates a new folder, `o` drops to a plain shell at the highlighted folder, `/` searches from here. |
| **Add / rename / edit in TUI** | `a` adds a new session via a 2-stage modal. `R` renames the workspace (edits the TOML `name` field). `e` edits existing session fields. |
| **Open in Terminal** | `o` in session list / tree mode / picker's reveal modal drops you into a plain shell at the chosen path — exits pa, no mpx, no session state. |
| **pa://** URL scheme | `pa open <url>` dispatches `pa://open/<path>`, `pa://workspace/<uuid>`, `pa://launch/<uuid>/<session>`, and `pa://shell/<path>` links. `pa protocol install` registers the scheme with the OS (Linux `.desktop`, Windows / WSL registry); works with any detected or user-specified terminal emulator. |
| **Cross-device** | `pa claim` takeover-attach. `pa launch --resume` appends `--continue` for claude-code sessions. |
| **Workspace scoping** | Session names prefixed with workspace name in the mpx (`my-project-shell`). Auto-re-register on walk-up (folder move resilience). |
| **Extras** | Declarative export (`pa export`), onboarding wizard, shell completions, bundled bash snippets, per-session env vars. |

**Still roadmapped**: Tags/Groups views, `pa up` eager-launch, datetime column, jump-back-to-pa from inside a session. See [ROADMAP.md](./ROADMAP.md).

## What it is

- A **TUI** — terminal-native. No GUI, no browser, no Electron. Runs over SSH without thinking about it.
- A **one-shot CLI** — same binary. `pa launch <name>` for scripted use; `pa` with no args for the TUI.
- **Portable** — single static Rust binary. `scp` it to a new machine and run. No runtime to install.
- A **workspace layer on top of the filesystem** — hierarchy on hierarchy.
- An **attach-or-create orchestrator** over tmux and zellij.

## What it is not

- Not a VS Code extension. Not a browser app. Not a daemon.
- Not an agent framework. It launches agents; it does not replace them.
- Not a project scaffolder (workspace scaffolding is built in; project-level is someone else's job).
- Not tied to one multiplexer.

## Building

```sh
cargo build                  # debug build
cargo nextest run            # unit + integration tests
bacon test                   # watch loop
cargo build --release        # release binary → target/release/pa
```

CI runs `fmt`, `clippy -D warnings`, `nextest`, and `build` on Linux + macOS for every push and PR.

## Docs site

```sh
cd docs
bun install && bun run serve       # interactive menu
bun run build                      # static build → docs/dist
```

Full docs: <https://cybersader.github.io/portagenty/>

## Tech decisions (locked)

| Decision | Choice | Why |
|---|---|---|
| Language | Rust | Trusted, secure, long-term maintenance. Single static binary. |
| Binary name | `pa` | Short, memorable, no major conflicts. |
| Config format | TOML | Rust-idiomatic, human-editable, comment-friendly. |
| Config scope | Three-tier: global + workspace + per-project | Registration at any level. |
| Multiplexers | tmux + zellij | Cross-platform headless detach/reattach. |
| Launch semantics | Workspace-scoped, lazy | Sessions spawn on first open, not on workspace entry. |
| State store | Split: TOML config + live mpx polling | No SQLite. Inspectable files + rebuilt volatile state. |
| Agent awareness | Agnostic core, optional `kind:` hint | `pa` doesn't parse agent state; hint unlocks integrations. |

## Non-goals

- **No project scaffolding.** Workspace scaffolding is built in; project-level scaffolding is a separate tool's job.
- **No remote-machine awareness.** SSH in and run `pa` there.
- **No agent-API wrapping.** Claude Code / OpenCode are external processes.
- **No SQLite.** State is TOML files plus live polling.
- **No GUI.** Terminal only. Forever.

## Related

- **[agentic-workflow-and-tech-stack](https://github.com/cybersader/agentic-workflow-and-tech-stack)** — my scaffold + knowledge base for filesystem-based AI agent workflows. portagenty is its stratum-2 launcher. See [stack / 02-terminal](https://cybersader.github.io/agentic-workflow-and-tech-stack/stack/02-terminal/) for how `pa` composes with WezTerm + Zellij + Claude Code in the broader agentic-coding setup, and [terminal emulator stack research](https://cybersader.github.io/agentic-workflow-and-tech-stack/agent-context/zz-research/2026-04-18-terminal-emulator-stack/) for the layer model portagenty sits inside.