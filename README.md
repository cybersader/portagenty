# portagenty

A portable, terminal-native launcher for agent workspaces. Written in Rust. Binary name: `pa`.

> Status: pre-release, **chunk A bootstrap landed**. Cargo project, CI, docs site, and module skeleton are in place; behavior is not. The TUI and tmux adapter come in chunks B–D. See [DESIGN.md](./DESIGN.md) for the architectural deep-dive and [ROADMAP.md](./ROADMAP.md) for what gets built when. Docs site: <https://cybersader.github.io/portagenty/>. Local + Tailscale preview also supported (see below).

## Building (development)

```sh
cargo build                  # debug build
cargo nextest run            # fast unit + integration tests
bacon test                   # watch loop, sub-second feedback
cargo build --release        # release binary at target/release/pa
```

## Docs site (local + Tailscale share)

```sh
cd docs
bun install                  # first time only

bun run serve                # interactive menu (dev / preview / build / tailscale / cloudflare)
bun run serve:dev            # Astro dev with --host 0.0.0.0 (LAN/Tailscale-bindable)
bun run serve:tailscale      # serves via `tailscale serve` and prints the share URL + tailnet IP
bun run serve:cloudflare     # public Cloudflare Tunnel (when you actually want it public)
bun run build                # static build → docs/dist
```

The `serve.mjs` orchestrator handles the WSL ↔ Windows rollup-binary contamination problem (auto-reinstalls when it detects a wrong-OS install). Pattern lifted from `cybersader/crosswalker`.

CI runs `fmt`, `clippy -D warnings`, `nextest`, and `build` on Linux + macOS for every push and PR. A separate `docs-build` workflow verifies the docs build on `docs/**` changes — no deploy, since the repo is private.

## The itch

VS Code + [terminal-workspaces](https://github.com/cybersader/vscode-terminal-workspaces) works, but it's a heavy GUI wrapping what is really a terminal + agent workflow. The sidebar is the useful 5%. Everything else is weight.

Separately, [agentic-workflow-and-tech-stack](https://github.com/cybersader/agentic-workflow-and-tech-stack) gives a solid filesystem-based scaffold (skills, agents, hooks, temperature-gradient KB) for running Claude Code / OpenCode across machines via Tailscale + tmux/zellij. It also works, but the entry point is still "`cd` somewhere and type `claude`."

portagenty is the replacement launcher and the missing workspace layer — the thing that tells you what agent sessions exist, where, in what state, and spins them up.

## What it is

- A **TUI** — terminal-native. No GUI, no browser, no Electron. Runs over SSH without thinking about it.
- A **one-shot CLI** — same binary. `pa launch <name>` for scripted use; `pa` with no args for the TUI.
- **Portable** — single static Rust binary. `scp` it to a new machine and run. No runtime to install.
- A **workspace layer on top of the filesystem** — see the key idea below.
- An **attach-or-create orchestrator** over tmux, zellij, and WezTerm.

## What it is not

- Not a VS Code extension. Not a browser app. Not a daemon with a web dashboard.
- Not a new agent framework. It launches existing agents; it does not replace them.
- Not a scaffolder. Creating new projects is a separate tool's job.
- Not an Obsidian plugin or note tool.
- Not tied to one multiplexer. tmux, zellij, and WezTerm are all first-class.

## The key idea: hierarchy on top of hierarchy

A filesystem forces every project into exactly one parent directory. But you don't think about your projects that way. You think:

- "What have I touched recently?" (ordering by recency)
- "Show me everything tagged `agentic`." (tagged views)
- "These five repos are my current focus — group them." (curated playlists)

Today, existing tools force you to pick one view: the filesystem. portagenty layers **alternative hierarchies** over that one ground-truth hierarchy, without moving anything on disk:

- Projects live wherever they live. portagenty never moves or symlinks them.
- A **workspace** is a named, curated view — a query, a tag set, or an ordered list — over real project paths.
- The same project can appear in many workspaces.
- Workspaces themselves are first-class files: findable, shareable (committable), diff-able.

The filesystem is storage. portagenty is the index and launcher on top.

## Tech decisions (locked)

| Decision | Choice | Why |
|---|---|---|
| Language | Rust | Trusted, secure, long-term maintenance. Single static binary. |
| Binary name | `pa` | Short, memorable, no major conflicts. |
| Config format | TOML | Rust-idiomatic, human-editable, comment-friendly, great tooling. |
| Config scope | Three-tier: global + workspace + per-project | No single source of truth; registration can happen at any level. |
| Multiplexers (tier-1) | tmux, zellij, WezTerm | Cross-platform coverage. WezTerm unlocks Windows-native persistence. |
| Multiplexer strategy | Imperative (primary) + declarative export (secondary) | Lazy creation works naturally imperatively; layouts as export for commit/share. |
| Launch semantics | Workspace-scoped, lazy | Entering a workspace primes definitions; panes spawn on first open. |
| State store | Split: TOML config + live mpx polling | No SQLite. No migrations. Durable state lives in files; volatile state is rebuilt. |
| Platforms | Linux, macOS, Windows native | WSL still supported for Windows users who prefer tmux/zellij there. |
| Agent awareness | Agnostic core, optional `kind:` hint | `pa` doesn't parse agent state by default; hint unlocks integrations. |

## Relation to sibling repos

| Repo | Role |
|---|---|
| `agentic-workflow-and-tech-stack` | Defines *what* an agent-ready project looks like (skills, hooks, KB). portagenty is a good launcher for projects scaffolded by it, but does not require it. |
| `vscode-terminal-workspaces` | The thing portagenty is replacing for the author's personal workflow. Will remain maintained for VS Code users. |

## Non-goals for v1 code

- **No scaffolding.** Creating projects is out of scope; that's `setup.sh` (in agentic-workflow) or a future purpose-built tool.
- **No remote-machine awareness.** portagenty runs on the machine you're on. It doesn't ping other machines. SSH into them and run `pa` there.
- **No agent-API wrapping.** Claude Code / OpenCode are external processes. portagenty launches them; it does not embed or proxy their APIs.
- **No SQLite or embedded DB.** State is in TOML files plus live polling. If that ever becomes a bottleneck we'll revisit — not before.
- **No GUI.** Terminal only. Forever.
