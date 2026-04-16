# portagenty — DESIGN

Architectural deep-dive. Companion to [README.md](./README.md) (vision) and [ROADMAP.md](./ROADMAP.md) (sequencing). This file is the source of truth for terminology and architectural decisions.

Status: v1.x implementation. Schemas shown here are the committed on-disk formats unless explicitly labeled as "future" or "deferred".

---

**Table of contents**

1. Vocabulary
2. The three-tier config model
3. Workspace file: discovery and shape
4. State model — the "split" approach
5. Multiplexer adapters
6. Launch model — workspace-scoped, lazy
7. Agent integration — agnostic core
8. Platform notes
9. Untracked session adoption
10. Termux and small-screen TUI constraints
11. WSL ↔ native Windows sync — scope
12. Entry-point behavior — what `pa` does from any directory
13. Explicitly out of scope

---

## 1. Vocabulary

These terms mean exactly one thing across the whole project. If you find yourself using one of them loosely, re-read this section.

- **Project** — a directory on disk with code or content you work on. Registered with portagenty at any of three tiers (global, workspace, per-project). A project is identified by its filesystem path.
- **Session** — one unit of execution: a shell, a process, an agent. Defined by *name + cwd + command*. A session belongs to a workspace.
- **Workspace** — a named, curated view over one or more projects plus the sessions you use to work on them. A first-class file on disk, designed to be committable. A workspace is where "hierarchy on top of hierarchy" happens.
- **View** — a specific ordering/filtering of projects presented in the TUI. Three planned: Recently Opened (LRU), Tags, Custom Groups. v1.x ships Recently Opened (picker + session-list). Tags and Custom Groups are roadmapped. Views are composable with workspaces.
- **Multiplexer** (mpx) — tmux or zellij. The thing that actually owns terminal panes and keeps them alive across detaches. portagenty drives it; it does not replace it. **WezTerm is intentionally not supported** — its mux model is built around the GUI terminal's own windowing rather than the headless detach/reattach pattern portagenty depends on. See ROADMAP v1.x for the full rationale. The `Multiplexer::Wezterm` enum variant still exists so workspace files don't fail-parse, but `build_mux` returns a clear "use tmux or zellij" message.
- **Adapter** — code inside portagenty that speaks to one specific multiplexer. v1 shipped tmux as the reference adapter; v1.x added zellij. (A future "agent adapter" concept is deferred — see §7.)
- **Profile** — *deferred to v1.x.* A named bundle of session defaults (shell, env, mpx settings) that multiple sessions can inherit. Lifted from `vscode-terminal-workspaces`. Not in v1.

---

## 2. The three-tier config model

Project registration and session definitions can be declared at three layers. Any tier can register something; the TUI merges them.

| Tier | Location | Typical contents | Committable? |
|---|---|---|---|
| **Global** | `$XDG_CONFIG_HOME/portagenty/config.toml` | Known projects you want visible everywhere; default multiplexer; user preferences. | No — personal/machine |
| **Workspace** | A `*.portagenty.toml` file anywhere | The workspace itself: its projects, sessions, tags, custom groups. | **Yes** — primary use case |
| **Per-project** | `portagenty.toml` at a project root | Sessions defined by the project's own repo ("start dev server" etc.) so anyone cloning gets them. | **Yes** |

**Merge rules** (v1, may evolve):

1. Global registry lists projects; per-project files under those paths augment with sessions.
2. When a workspace file is "entered," its session list replaces/augments what the global+per-project combination would have shown. The workspace is the active view.
3. Conflicts (two tiers defining a session with the same name) resolve: **workspace wins over per-project wins over global**. Closer to the user's current intent = higher priority.

**Example** (sketched, not final):

```
# Global:  ~/.config/portagenty/config.toml
default_multiplexer = "tmux"

[[project]]
path = "~/code/portagenty"
tags = ["rust", "agentic"]

[[project]]
path = "~/code/cyberbase"
tags = ["obsidian"]
```

```
# Workspace: ~/workspaces/agentic-stuff.portagenty.toml
name = "Agentic stuff"
multiplexer = "zellij"   # overrides global default
groups = [["portagenty", "agentic-workflow-and-tech-stack"]]

[[session]]
project = "~/code/portagenty"
name = "claude"
cwd = "."
command = "cc"
```

```
# Per-project: ~/code/portagenty/portagenty.toml
[[session]]
name = "tests"
cwd = "."
command = "cargo watch -x test"
```

### Registry vs. gatekeeper

The global `[[project]]` list is an **index**, not a gatekeeper. A
workspace file can declare `projects = [...]` containing paths that
have never been globally registered; the workspace loads fine, and
those projects show up normally in the session list. Global
registration exists to power cross-workspace views (the picker's
recency sort, tag grouping, future custom-group curation) — it is
not a requirement for a workspace to reference a project.

The symmetric rule for sessions: the merge precedence (workspace >
per-project > global) applies when the *same* session name appears at
multiple tiers. A session declared only in a per-project file is
still visible in any workspace that `projects = [...]`-lists that
project's directory. Registration is additive across tiers, conflict
resolution is precedence-based.

---

## 3. Workspace file: discovery and shape

Workspace files are the interesting layer. They're how you commit a workspace definition so someone else (or future-you on another machine) can pick it up.

**Three ways to find them, all coexist:**

1. **Walk-up from `$PWD`** — like `.git` discovery. Starting from where you invoke `pa`, walk upward looking for a file matching `*.portagenty.toml`. First match wins. Good for "I'm `cd`'d into my workspace; just use this one."
2. **Global registry** — the TUI home screen shows workspaces listed in `~/.config/portagenty/config.toml`. These are the workspaces you use often.
3. **Explicit path** — `pa open ./path/to/foo.portagenty.toml`. For scripting, for trying someone else's workspace without registering it.

**Opportunistic search** — if `fd` (Linux/mac/Windows) or Everything CLI (Windows) is on `PATH`, `pa` may use them for fast recursive discovery inside the TUI's "find workspace" action. Detect at runtime; never hard-depend.

**Commitability rules** — a workspace file is designed to be checked in. To make that safe:

- No absolute paths. Project references use paths relative to the workspace file, or a path template like `${HOME}/code/foo`.
- No LRU, no last-attached timestamps, no machine-specific state. Those live in the split state store (§4).
- No usernames, API keys, or machine identifiers.

If a user needs machine-specific overrides (say, two laptops with different project locations), the pattern is: workspace file committed to git, plus a tiny local override file alongside it (gitignored). v1 ships without formal support for overrides; they'll be added if needed.

---

## 4. State model — the "split" approach

portagenty splits what it knows into two categories:

**Durable, user-authored**: lives in TOML files (§2).

- Project registrations
- Workspace definitions
- Session definitions
- Tags, groups, preferences
- Multiplexer choices

**Volatile, machine/time-local**: lives in `$XDG_STATE_HOME/portagenty/state.toml` (or equivalent on each OS).

- LRU of recently opened workspaces / projects / sessions
- Last-attached timestamps
- User's most recent TUI view preference

**Live, rebuilt on every run**: not persisted anywhere.

- Which tmux/zellij/WezTerm sessions are currently running. Obtained by polling the mpx CLI at startup and on refresh. This is how untracked sessions get surfaced (see §5).
- Current focus / selection in the TUI.

**Why no SQLite?** v1 doesn't need query performance. It needs inspectability, simple atomic writes, and no migration pain. A SQLite file adds a library, a schema, a migration story, and makes the state opaque. If v2+ ever needs cross-process concurrency or fast queries across thousands of tagged projects, revisit. Until then: files.

**Concurrency** — if someone runs two `pa` TUIs at once, the volatile state file is subject to last-writer-wins on LRU updates. That's fine. If it ever causes user-visible corruption, add advisory file locks. Don't pre-optimize.

---

## 5. Multiplexer adapters

v1 shipped the tmux adapter as the reference baseline. v1.x added zellij. WezTerm is intentionally not supported (see §1 Vocabulary and ROADMAP v1.x for the rationale). Both shipping adapters present the same core interface to the rest of portagenty, with capability flags for features only one mpx supports.

**Core interface** (conceptual, not Rust API):

- `list_sessions()` — return live sessions the adapter knows about, including ones portagenty didn't launch.
- `session_exists(name)` / `attach(name)` / `create_and_attach(name, cwd, command)` — the attach-or-create loop.
- `kill(name)` — close a session.
- `detach_current()` — let user step back to the TUI.
- `export(workspace) -> artifact` — optional; produces a native artifact (KDL layout for zellij, a shell script for tmux, whatever for WezTerm).

**tmux** — the reference adapter. The stable baseline.

- Session-per-workspace, window-per-session model.
- Attach-or-create is a shell pipe: `tmux has-session -t NAME 2>/dev/null && tmux attach-session -t NAME || tmux new-session -s NAME -c CWD -d`.
- Session/window naming: sanitize `[^a-zA-Z0-9_-]` → `_`, clamp at 50 chars. Match the VS Code extension's approach so existing sessions carry over.
- Untracked session adoption: `tmux list-sessions -F '#{session_name}|#{session_path}|#{session_attached}'`.

**zellij** — added in v1.x.

- Zellij's model is different: layouts (KDL) define tabs + panes declaratively. Opening a layout spins everything up at once — fights our "lazy" default.
- v1.x adapter runs imperative where possible (`zellij attach`, `zellij action new-tab`, `zellij action new-pane`). For workspaces where the user wants "all at once," `pa export` produces a KDL layout they can open normally.
- Works better with OpenCode than tmux does, per the agentic-workflow README.

**WezTerm** — added in v1.x, with honest caveats.

- WezTerm has a built-in mux server reachable via `wezterm cli`. The only modern, cross-platform, native-on-Windows mpx with persistent panes.
- **Known limitations**: its session-attach story is weaker than tmux's. Detaching and reattaching from arbitrary clients is rougher; some flows that feel seamless in tmux require explicit spawn/list plumbing in WezTerm. Users should expect WezTerm to be tier-1 in coverage, not tier-1 in polish. This will be called out in the TUI when WezTerm is the active adapter.

**Mpx choice resolution**:

1. If the current workspace declares `multiplexer = "..."`, use that.
2. Else use the global default from `config.toml`.
3. Else probe: if inside a tmux/zellij/WezTerm process already, use that. Else fall back to tmux if installed, then zellij, then WezTerm.

---

## 6. Launch model — workspace-scoped, lazy

"Entering" a workspace is cheap. It does not start any processes. It loads the session definitions, checks the multiplexer for live sessions matching those names, and draws the TUI.

When the user selects a session and hits Enter:

1. If the mpx already has a session with that sanitized name → attach to it. (Covers both "resume something I left running" and "adopt an untracked session.")
2. If not → create it with the session's `cwd` + `command`, then attach.

This is imperative, on-demand. A workspace with 20 sessions defined never costs you 20 processes worth of startup. It costs you one process when you open one session.

**Eager / "jump-in" flag** — a v1.x feature: `pa launch <workspace> --eager` or a config key that tells portagenty to spawn every session in a workspace at entry time, so long-running things (agents, dev servers) are warm by the time you tab to them. Off by default.

---

## 7. Agent integration — agnostic core

v1 does not know what Claude Code is. A session is a command; `command = "claude"` and `command = "vim"` are indistinguishable to the core.

That choice is deliberate. It's what keeps portagenty durable as the agent ecosystem shifts: new CLIs (Aider, Codex, something-else-in-six-months) don't require core changes. You just write a new session.

An optional `kind:` field on a session can unlock niceties later. Sketch:

```
[[session]]
name = "claude"
cwd = "."
command = "cc"
kind = "claude-code"    # v1.x hook; ignored in v1
```

Planned `kind:` values: `claude-code`, `opencode`, `shell`, `editor`, `dev-server`. Effects (when implemented): agent-running indicators in the TUI, smart resume (`--continue` flags), session-coloring.

A full plugin runtime — where third-party adapters register themselves via some extension mechanism — is a v2+ question. Not a v1 problem.

---

## 8. Platform notes

| Platform | mpx options | Notes |
|---|---|---|
| **Linux** | tmux ✅, zellij ✅, WezTerm ✅ | Primary dev environment. Everything should just work. |
| **macOS** | tmux ✅, zellij ✅, WezTerm ✅ | Same as Linux. |
| **WSL on Windows** | tmux ✅, zellij ✅ | For users running portagenty inside WSL. WezTerm not the obvious choice here. |
| **Windows native** | WezTerm ✅, tmux ❌, zellij ⚠️ | tmux doesn't run natively; zellij is unstable on Windows. WezTerm is the story. WezTerm's attach limitations (§5) apply here. |

If the user's chosen multiplexer isn't installed, portagenty exits with a clear error and a link to installation instructions. It doesn't silently fall back to running raw terminals — that would hide the problem.

---

## 9. Untracked session adoption

A central feature, carried over from `vscode-terminal-workspaces`.

At startup and on TUI refresh, each active adapter is asked for its live sessions. Any session not referenced by the current workspace's definitions is surfaced in a distinct **"Untracked"** area of the TUI. From there the user can:

- **Attach** — same as attaching to a known session.
- **Import** — add the session's `cwd` + a derived command guess as a new session in the current workspace.
- **Ignore** — dismiss from the view until next refresh.

This bridges the gap between "what portagenty thinks is going on" and "what's actually running." It's how someone who started a tmux session outside portagenty still gets it in their TUI.

---

## 10. Termux and small-screen TUI constraints

A primary access path for this tool is **Termux on Android → SSH → desktop → zellij → `pa`**. Portagenty never runs *on* Termux; it renders *through* it. But Termux imposes real constraints on what the TUI can assume:

**Keyboard reality**: Termux has no physical Ctrl/Alt/Meta. The on-screen Extra Keys row provides Ctrl/Esc/Tab/arrows as taps (each is a second tap on top of any letter). Volume-Down often maps to Ctrl, Volume-Up to Esc, but not everyone configures it. Flow control (Ctrl+S / Ctrl+Q) freezes the terminal if not disabled.

**Screen reality**: 15–25 visible rows on a typical phone in portrait. Narrow width. Mouse/touch is imprecise and awkward.

**Design rules that follow from this**:

- **Single-letter keybindings for all common actions.** Vim-flavored because the primitives are letters. `j`/`k` navigate, `Enter` opens, `q` quits, `/` searches, `?` shows help, `gg`/`G` jump, `tab` switches pane when present.
- **Avoid Alt/Meta in default bindings.** Any Alt-dependent shortcut is second-class and mirrored by a non-Alt equivalent.
- **`Esc` is always "back one level" or "cancel."** Never "close the app."
- **Responsive layout by `$LINES`/`$COLUMNS`.** Single-column list view is the default. Two-pane (list + detail) is opt-in for wider terminals and kicks in automatically at a threshold.
- **Mouse is never required.** Every action reachable by keys.
- **Short text labels, column eliding.** Do not hard-assume 80 columns.
- **Plain ANSI only.** No sixel, no alternate-screen tricks that mangle through SSH + zellij + Termux.
- **Assume flow control is disabled.** Call it out in setup docs; do not rely on Ctrl+S for anything.

The net effect: a TUI that feels generous on a desktop but remains one-handed-on-a-phone usable over SSH. That's the bar.

## 11. WSL ↔ native Windows sync — scope

Claude Code stores its sessions under `~/.claude/projects/<path-encoded-cwd>/`, where the encoding differs between environments (`-mnt-c-Users-X-project` on WSL, `C--Users-X-project` on PowerShell). `--resume` and `--continue` only find sessions from the current environment's encoding. Similarly, portagenty's global config under `$XDG_CONFIG_HOME/portagenty/` resolves to different paths on WSL vs. Windows native.

**portagenty's position**:

- **Workspace files and per-project `portagenty.toml` are designed to commit** and use relative paths (§2, §3). They cross environments trivially via git. No sync work needed there.
- **Global config is machine-local by design.** Users who want the same registry across WSL and Windows native handle it themselves (Syncthing, dotfiles repo, symlink into a shared mount). Portagenty does not ship a sync daemon.
- **Claude-session sync is not portagenty's problem to solve.** It's a Claude Code storage concern. If/when a tool solves it well, portagenty can reference it from docs but never embeds the logic.

A **future `kind:` hint plus adapter** (see §7 and ROADMAP v2+) could, in principle, know how to present "your most-recent Claude session from the other environment" — but that's a consumer of whatever cross-env session sync tool exists, not a replacement for it. Path-encoding translation belongs in a purpose-built tool like `claudecode-project-sync` (in `agentic-workflow-and-tech-stack`) or a successor that's had more eyes on it than we have.

## 12. Entry-point behavior — what `pa` does from any directory

The `pa` command is a single entry point that fans out based on
discoverable state. The rules below are the contract; everything else
in the code should derive from this table, not second-guess it.

### Decision order

When the user runs bare `pa` (no subcommand, no `-w`):

1. **Try walk-up discovery from `$PWD`.** If a `*.portagenty.toml`
   (with non-empty prefix) exists in the current directory or any
   ancestor, load it and open the session-list TUI for that workspace.
   This is the fast path: in-tree invocation should feel instant and
   local.
2. **Walk-up fails + non-interactive shell (pipe, script, CI).** Emit
   the discovery error and exit non-zero. Never prompt. Scripted
   callers must get deterministic behavior.
3. **Walk-up fails + interactive + first-time user** (no
   `.onboarded` sentinel in `$XDG_STATE_HOME/portagenty/`). Run the
   onboarding wizard (`src/onboarding/`). On scaffold, retry the load
   and continue into the session-list TUI. On "skip" or "show docs",
   exit cleanly.
4. **Walk-up fails + interactive + returning user.** Open the
   workspace picker TUI (`src/tui/picker.rs`). Lists every
   `[[workspace]]` registered in `$XDG_CONFIG_HOME/portagenty/config.toml`,
   filtering entries whose files no longer exist. Always includes a
   trailing **"live sessions on this machine"** option for the
   no-workspace case. On pick: load the chosen workspace inside the
   same ratatui session (no flicker) and continue into the
   session-list TUI.
5. **Walk-up fails + interactive + returning user + no registered
   workspaces.** Picker shows only the "live sessions" option, which
   resolves to a synthetic empty workspace populated from
   `mux.list_sessions()`.

The crucial consequence: **`pa` is callable from anywhere**. There is
no "right" directory to be in. Walk-up is an optimization, not a
requirement.

### Workspace registry — invariants

- **Auto-registered on scaffold.** Both `pa init` and the onboarding
  wizard append the new workspace to the global `[[workspace]]` list
  via `config::register_global_workspace`. Idempotent: re-runs and
  duplicate paths are no-ops. Preserves the rest of the config via
  `toml_edit`.
- **Never edited silently outside those paths.** Running `pa` or
  `pa launch` never mutates the registry. State drift only happens
  when the user explicitly scaffolds.
- **Stale-tolerant.** `config::list_registered_workspaces` filters
  entries whose `path` doesn't resolve to an existing file. The TUI
  never shows dead rows; the config file may retain them until the
  user cleans up (future `pa workspaces prune` is roadmap).
- **Absolute, canonicalized paths.** Entries are stored as absolute
  paths so they survive cwd changes. `~`/`${VAR}` expansion is done
  at read time, not write time, so env changes don't invalidate
  entries.

### UI contract

- **No stdin text prompts after onboarding.** Anything requiring user
  choice past the first-run wizard happens in ratatui. Stdin prompts
  were tried and reverted — they broke the TUI feel and made the app
  look half-finished.
- **One `ratatui::init()` / `restore()` bracket per invocation.**
  Picker → session-list transitions inside a single live terminal.
  Mux hand-off (create/attach) happens only after `restore()`.
- **Pre-launch banner on every mux hand-off** (§5 already implied
  this). Tells the user which session they're entering and the mpx-
  specific detach chord. This is information-only; we never rebind
  keys in the user's mpx config — that opinionated work lives in the
  `agentic-workflow-and-tech-stack` sibling repo.

### Navigation — Android-back semantics

The workspace picker is the **home screen**. Navigation has exactly
one back-stack and `Esc` means the same thing everywhere:

- **Session TUI**: `Esc` → picker. Always. Regardless of whether the
  user entered via walk-up, wizard-scaffold, or picker. There is no
  "entry-path mode" to track.
- **Picker (home)**: `Esc` or `q` → exit `pa`. One more Esc from home
  closes the app, analogous to Android's back button on a launcher.
- **Session TUI**: `q` / `Ctrl+C` → exit `pa` directly, bypassing the
  picker. Useful when the user knows they're done.
- **Enter** always opens the highlighted row. On a workspace row
  (picker) this loads the workspace and drops into its session TUI.
  On a session row (session TUI) this attaches or create-and-
  attaches via the mux.

Entry-flow-specific wrinkles:

- Walk-up-entered users still see the picker once they press Esc.
  This is a feature: the picker doubles as a "jump to another
  registered workspace" affordance without needing to exit `pa` and
  cd somewhere else first.
- If the picker's own registry is empty (no workspaces registered,
  no pinned mpx with live sessions) the picker still renders with
  the "live sessions on this machine" option as the only row, so
  Esc-to-home never leaves the user at a dead end.

### Nested-mpx refusal

Running `pa` inside a client of its own target multiplexer is a
guaranteed foot-gun (zellij refuses nested sessions entirely; tmux
allows them but it's rarely what the user wants). Handled by:

- Detecting `ZELLIJ_SESSION_NAME` / `ZELLIJ` / tmux `$TMUX` at load
  time.
- Refusing *before* opening the TUI with a message that names the
  current session and the correct detach chord. Letting the TUI open
  and then erroring post-restore buries the error in shell scrollback.

### Picker actions inventory

The picker is the home screen, so it carries a richer key vocabulary
than just navigation. As of the find-folder commit chain
(`piped-sauteeing-breeze.md`), keys are:

| Key | Action |
|---|---|
| `j` / `↓` / `k` / `↑` | move highlight (`g` / `G` jump to ends) |
| `Enter` | open the highlighted workspace |
| `n` | open the in-TUI find-folder + scaffold overlay |
| `d` | unregister the highlighted workspace from the global index (file stays on disk) |
| `D` | delete the workspace file *and* unregister (with confirm) |
| `r` | reveal the workspace path in a sticky info modal that auto-copies to clipboard |
| `?` | open the help overlay |
| `q` / `Esc` | exit pa (Esc dismisses a status line first) |

### Find-folder + scaffold flow (`n`)

Triggered by `n` in the picker. Implementation in `src/tui/find.rs`
+ `src/find/`. Behavior:

1. Overlay opens centered. Empty input shows recents + zoxide
   candidates immediately (instant, no FS walk).
2. Each typed character re-runs the tier orchestrator: recents +
   zoxide + plocate/locate/Everything CLI + fd + stdlib walker.
   Each tier silently skips if its underlying tool isn't installed.
3. Results are deduped on canonical path, then ranked against the
   query by `nucleo` (smart-case + smart-normalization). Top N
   surface. Zero-score entries are dropped so the list always has
   meaningful matches.
4. `Enter` on a highlighted candidate classifies:
   - Folder already contains a `*.portagenty.toml` → picker exits
     with that workspace as the outcome (it'll open immediately).
   - Folder has no workspace → confirm modal: "scaffold a new
     workspace at <path>?" `y` calls `crate::scaffold::create_at`
     with the dir's basename + machine-default mpx + no Claude
     session, registers globally, and the picker exits with the
     new workspace as the outcome.
5. The new workspace's session TUI loads immediately (per the
   user-locked decision: no extra "open it now?" prompt).

Special query forms:
- Empty → recents + zoxide only, no walks.
- Starts with `/` or `~/` → walk-from-prefix mode (stdlib walker
  only, scoped to the nearest existing ancestor).

Search backend tiers, fastest-first, all silent-skip if absent:
recency (state.toml) → zoxide → plocate / locate / Everything CLI →
fd → stdlib walker. All shell-outs run with a 1-second hard
timeout. No new C deps; only `nucleo-matcher` was added (pure Rust,
~50 KB).

### What this rules out

- A "persist the last workspace and auto-open it on bare `pa`" mode —
  ambiguous with walk-up, and breaks "call from anywhere".
- A CLI flag to *pre-select* a workspace in the picker. Scripts that
  know their target use `pa -w <path>` or a direct subcommand; they
  don't need the picker.
- Reaching for a daemon or cache to speed up repeated runs. Walk-up
  of a `*.portagenty.toml` is O(depth) filesystem reads; on any real
  system it's sub-millisecond. No invalidation problems to solve.
- Building our own filesystem index. The find pipeline piggybacks
  on plocate / Everything when present; we never spin up an indexer
  process or write to one. Aligns with §5's anti-daemon stance.

## 13. Explicitly out of scope

- **Scaffolding**. A tool for that exists (`agentic-workflow-and-tech-stack`'s `setup.sh`). Integration with a purpose-built scaffolder may happen later via a separate `pa new` subcommand that shells out.
- **Remote-machine awareness**. If you want portagenty on another machine, SSH in and run `pa` there. No mesh, no discovery, no RPC.
- **Agent-API wrapping**. Claude Code / OpenCode are launched as subprocesses. portagenty never speaks their APIs.
- **GUI / web UI**. Terminal only.
- **Syncing across machines**. Rely on git (for workspace files) and Syncthing/rsync (for anything else the user wants to sync). Not portagenty's problem.
- **Supervisor-mode agent management**. portagenty launches agents; it does not restart them, monitor their logs, or gather telemetry.
