---
title: Commands reference
description: Every subcommand pa ships, with flags and examples.
sidebar:
  order: 1
---

All commands respect the walk-up workspace discovery — run any of
them from anywhere under your workspace file and `pa` will find it.
The `-w`/`--workspace` flag overrides the walk-up with an explicit
path.

## `pa [path]`

Opens the TUI. With no arguments, walks up from `$PWD` for a
`*.portagenty.toml` or (if none) shows the workspace picker. Pass an
optional positional `path` to jump straight to a workspace without
needing to `cd` there first. The path can be a `*.portagenty.toml`
file or a directory (walks up from it).

```sh
pa                                    # walk up / show picker
pa ~/code/myproject                   # open workspace at this dir
pa ~/code/my.portagenty.toml          # open this workspace file directly
```

### Workspace picker (home screen)

Shown when `pa` runs outside any walkable workspace, or after
pressing `Esc` from the session list.

| Key | Action |
|---|---|
| `j` / `↓` | Next workspace |
| `k` / `↑` | Previous workspace |
| `g` / `Home` | First workspace |
| `G` / `End` | Last workspace |
| `Enter` / `l` / `→` | Open the highlighted workspace |
| `Ctrl+D` / `Ctrl+U` | Half-page down / up |
| `PgDn` / `PgUp` | 10-row jumps |
| `n` | Find a folder + scaffold a new workspace |
| `/` | Filter: type to fuzzy-match name / path / tag; `Esc` clears |
| `f` | Cycle the tag filter (none → tag → … → none) |
| `t` | Edit the highlighted workspace's tags (comma-separated) |
| `R` | Rename workspace (edits TOML `name` field) |
| `r` | Reveal workspace path (auto-copies; press `o` inside to open shell there) |
| `a` | Archive workspace — hide it from this list (reversible). In the archived view, `a` unarchives instead. |
| `A` | Toggle the archived-workspaces view |
| `d` | Unregister workspace from global index (file stays on disk) |
| `D` | Delete workspace file and unregister (with confirm) |
| `X` | Preview and stop workspace sessions: verified owned workloads use graceful + non-force systemd stop, unmanaged targets use mpx-native kill, and stale/ambiguous receipts are skipped. No bulk force escalation. |
| `Ctrl+R` | Refresh live-session counts |
| `?` | Help overlay |
| `q` / `Esc` | Exit `pa` |

### Archiving workspaces

Long registries get noisy. Press `a` on any workspace to **archive**
it — it disappears from the picker's main list but stays registered
and on disk. Press `A` to toggle into the archived view, where you
can `a` a workspace to bring it back. Archiving is purely a
per-machine display preference (stored as `archived = true` on the
`[[workspace]]` entry in your global config); it never touches the
workspace TOML or its files, and archived workspaces are still
reachable by `pa <path>` and `pa://` links.

### Filtering + tags

Two more ways to cut through a long list:

- **`/` filter** — start typing to fuzzy-match the list live by name,
  path, and tags (case-insensitive subsequence). The recency order is
  preserved (it filters, it doesn't re-rank). `Enter` opens the
  highlighted match; `Esc` clears the query then exits filter mode.
  `Ctrl+F` is an alias for `/`.
- **`t` tags** — tag the highlighted workspace with a comma-separated
  list; it's written to the committable `tags = [...]` field in the
  workspace TOML (so tags travel with the file). Tags show as dim
  `#chip`s on each row.
- **`f` tag-filter** — cycle a single-tag view filter (none → most
  common tag → … → none). The title bar shows the active `#tag`.

A workspace also inherits any tags its registered projects carry in
the global `[[project]]` registry (see the schema reference).

### Mouse (opt-in)

Press `M` in the picker to toggle **mouse mode** (persisted as
`[ui] mouse = true` in your global config). When on:

- **wheel** scrolls the selection,
- **left-click** selects the row under the cursor,
- **double-click** (same row, <400ms) opens it.

Off by default, and picker-only on purpose: enabling terminal mouse
capture **disables your terminal's own click-drag text selection**
of the paths shown in rows, so the session list is deliberately left
mouse-free (native copy keeps working there). Capture is only active
while the picker is on screen. Keyboard navigation is always the
guaranteed path — mouse is never required (and Android/Termux report
mouse poorly, so don't rely on it there).

### Session list

| Key | Action |
|---|---|
| `j` / `↓` / `Alt+J` | Next session |
| `k` / `↑` / `Alt+K` | Previous session |
| `g` / `Home` | First session |
| `G` / `End` | Last session |
| `Enter` / `l` / `→` | Attach a live/owned exact target, attach a legacy-v1 or split target without granting resource control, require supervision with the complete `3G` / `5G` / `512MiB` / `800%` / `1200` baseline when starting an eligible idle UUID-backed row, directly relaunch any safely reconciled idle stale row through the existing exact signal-free coordinator, or ordinarily create a no-ID/invalid/unsupported idle row |
| `Ctrl+D` / `Ctrl+U` | Half-page down / up |
| `PgDn` / `PgUp` | 10-row jumps |
| `a` | Add a new session (2-stage name → command modal) |
| `e` | Edit session (name / cwd / command / kind / env) |
| `d` | Delete session from workspace TOML (with confirm) |
| `S` | Advanced/custom supervision. Edit the same recommended limits, confirm-add a UUID to a legacy row, customize a stale replacement, or confirm a terminate-then-fresh-relaunch flow for a live shared row. Existing process trees are never migrated or claimed; `S` never silently falls back ordinary. |
| `r` | Refresh resources for the selected owned workload |
| `x` | Owned row: confirm graceful + non-force stop. Stale row: confirm receipt-only cleanup after proving the exact unit and target are absent (no signal is sent). Unmanaged row: confirm mpx-native kill. |
| `X` | Separately confirm whole-cgroup SIGKILL for an owned-and-verified workload |
| `z` | Toggle expand-on-select (see below) |
| `m` | Switch workspace multiplexer (tmux ↔ zellij) |
| `t` | Open the file tree rooted at the workspace's directory |
| `o` | Open a plain shell at the workspace's directory (exits pa) |
| `?` | Help overlay |
| `Esc` / `q` / `Ctrl+Q` | Back to workspace picker |
| `Ctrl+C` | Exit `pa` directly |

A receipt-backed row is non-attachable until exact reconciliation finishes. A v2
owned row requires exact unit/InvocationID/cgroup/target plus workload
PID/start-time/nonce proof and bounded descendant containment. A live v1 row becomes
`legacy/restart`, attaches only to its exact target, and exposes no resource stop or
force-kill. A `split` row can also attach to its exact target but withholds
whole-workload metrics and control. `ambiguous` rows expose no action; a pending-launch journal blocks another supervised creation until its evidence is reconciled.
Optional launch/creator boot UUIDs are non-authoritative provenance: malformed,
missing, or unreadable values do not invalidate otherwise complete receipt state or
gate stale-row Enter.

If a receipt becomes `stale`, Portagenty never chases the old opaque target, and
`X` remains unavailable because there is no verified cgroup to force-kill. On an
idle declared row, a completed error-free stale reconciliation dispatches the
existing cleanup/relaunch coordinator directly. The coordinator remains authoritative:
it proves the stored receipt is unchanged, pending evidence is absent, the exact
systemd invocation and private multiplexer target are absent, and the durable marker,
capabilities, limits, and ordinary-target races are safe before removing anything or
creating a fresh supervised binding. It sends no signal to an old workload and may
still refuse if the evidence changes. Optional boot provenance does not gate Enter.
Pending, ambiguous, worker-error, and unreconciled evidence blocks Enter; split
containment attaches only to its exact private target. Prior nonempty limits are
reused; an empty prior policy resolves from the declared session kind. `S` exposes
those values for editing. `x` means confirmed cleanup-only on a stale row, confirmed
graceful/non-force stop on an owned row, and confirmed multiplexer-native kill on an
unmanaged live row; `X` remains separately confirmed force-kill. If a real ordinary
target is live beside the stale receipt, Enter attaches it normally instead of
cleaning, stopping, or claiming it. Portagenty does not launch stale rows at TUI
startup and provides no bulk startup relaunch.

### Expand-on-select

The highlighted session row expands in place to show its full
**description**, its **real command**, its **cwd**, and any available
supervised resource summary/details on labeled lines (`desc ▸` / `cmd ▸` /
`cwd ▸` / `res ▸`). Collapsed rows stay
one line, so the list stays scannable and only one row is ever tall.

This is also how you see the real command on an **annotated** row:
when a session has a `description`, the COMMAND column shows the
note instead of the command, so the `cmd ▸` expansion line is where
the actual launch command lives. The description is capped at 3
wrapped lines. Press `z` to toggle the whole behavior off for
maximum-density scanning (session-local; on by default).

### Find overlay (triggered by `n` in picker or `e → c` in session list)

| Key | Action |
|---|---|
| Type characters | Fuzzy-search folders by leaf name (nucleo ranking) |
| `↑` / `↓` | Move highlight through results |
| `>` / `→` | Drill into highlighted folder |
| `<` / `←` | Go up to parent folder |
| `Enter` | Select folder (scaffold or open existing workspace) |
| `Ctrl+R` | Toggle global search (all mount points / filesystem root) |
| `Ctrl+T` | Switch to tree-browse mode |
| `Ctrl+F` | Fullscreen path display |
| `Esc` | Close the find overlay |

### Tree browser (triggered by `Ctrl+T` inside find overlay, or `t` in session list)

| Key | Action |
|---|---|
| `j` / `↓` | Next row |
| `k` / `↑` | Previous row |
| `g` / `G` | First / last row |
| `Enter` | Select (file or leaf) |
| `l` / `→` / `Space` | Expand directory (inline) |
| `h` / `←` | Collapse directory |
| `.` | Drill — re-root the tree at the highlighted folder |
| `Backspace` | Pop root — re-root at the current root's parent |
| `n` | Create a new folder under the current root |
| `o` | Open a plain shell at the highlighted folder (exits pa) |
| `/` | Search from here — back to search mode with this folder as root |
| `Ctrl+T` / `Esc` | Back to search mode |
| `q` / `Ctrl+C` | Close the overlay |

## `pa launch <session>`

Attach to (or create-and-attach) a specific session by name, without
entering the TUI.

| Flag | Default | What |
|---|---|---|
| `-w`, `--workspace <path>` | walk-up | Explicit workspace file |
| `--dry-run` | off | Print what would happen, don't run it |
| `--shared` | off | Don't detach other clients (see [attach modes](../../concepts/#attach-mode-takeover--shared)) |
| `--resume` | off | Kind-aware resume. For `kind = "claude-code"` sessions, appends `--continue` before launch so Claude picks up its prior conversation. Other kinds print a one-line hint to stderr and launch unchanged. The workspace TOML command string is never mutated on disk. |
| `--fresh` | off | Kill any existing mpx session with this name before launching. On zellij this is the only way to guarantee other clients are disconnected (zellij doesn't support per-client takeover). On tmux the default takeover already handles it — use `--fresh` only when you specifically want to wipe running state and restart from the workspace's declared command. Owned supervised bindings refuse `--fresh`; stop them through `pa resources` instead. |
| `--supervise` | off | Experimental Linux-only creation as a fresh transient systemd user service with an exact private tmux/Zellij target and versioned cgroup-v2 ownership receipt. Existing shared sessions are never claimed. |
| `--memory-high <SIZE>` | kind-selected | Memory-reclaim threshold (`512M`, `3G`, etc.). Implies `--supervise`. Claude default: `3G`. |
| `--memory-max <SIZE>` | kind-selected | Hard memory ceiling. Implies `--supervise`. Claude default: `5G`. |
| `--memory-swap-max <SIZE>` | kind-selected | Hard swap ceiling. Implies `--supervise`. Claude default: `512MiB`. |
| `--cpu-quota <PERCENT>` | kind-selected | Aggregate CPU quota; values above 100 allow multiple cores. Implies `--supervise`. Claude default: `800`. |
| `--tasks-max <COUNT>` | kind-selected | Maximum task/thread count. Implies `--supervise`. Claude default: `1200`. |

Examples:

```sh
pa launch claude
pa launch claude --dry-run
pa launch claude -w ~/code/my.portagenty.toml
pa launch claude --shared            # leave other devices attached
pa launch claude --resume            # claude-code → appends --continue
pa launch claude --supervise
pa launch claude --memory-high 2G --memory-max 4G --memory-swap-max 256MiB \
  --cpu-quota 600 --tasks-max 1000
```

Resource-limit flags imply supervision. Explicit CLI supervision requires a valid
workspace UUID and a supported Linux systemd-user/cgroup-v2 environment; it never
falls back ordinary. Every supervised kind resolves missing fields from the complete
`3G` MemoryHigh, `5G` MemoryMax, `512MiB` MemorySwapMax, `800%` CPU, and `1200`
task baseline. Exactly `kind = "claude-code"` additionally selects
`claude-code.slice`, resume behavior, and oomd metadata; a session merely named or
commanded `claude` stays generic. Claude fields cannot be cleared and overrides must
be equal to or stricter than the baseline, with MemoryHigh not exceeding MemoryMax.
Generic sessions stay in normal user-manager placement with the same finite baseline.

Before a Claude-kind service is created, Portagenty verifies that the externally
provisioned aggregate slice is structurally beneath `/claude.slice/claude-code.slice`
with finite positive memory high/max, swap max, CPU quota, and
`ManagedOOMPreference=omit`; aggregate `TasksMax` is optional and may be infinity,
while every Claude service still receives finite `TasksMax`. Portagenty never
creates or modifies the slice. Eligible UUID-backed routine TUI Enter is
supervision-required: unavailable capability/runtime, receipt ambiguity, identity or
target races, and every failure after creation may have begun return to the same
actionable row without creating an ordinary target. Pending launches block attach,
fallback, creation, stop, and kill until reconciled. A valid unequal stored/current
boot UUID proves only that the pending creator is gone; otherwise PID/start-time
proof remains required. Cleanup requires exact unit and private-target absence. An
absent marker is already clean; an exact marker may be removed only after its full
owner-runtime identity is revalidated and its recorded anchor PID/start time is
proven dead. Probe errors, live anchors, mismatches, and partial presence stay
ambiguous. Private tmux servers are launched without user-bus/runtime variables to
prevent sibling tmux scopes, while the pane receives the exact restored user bus.

`S` edits all five limits or resolves cleared fields from the declared kind, can
confirm-add a UUID to a writable legacy workspace, and can separately confirm
termination of one exact live shared target before a fresh supervised launch. It
does not migrate or retroactively claim the running process tree. Supervised Zellij
layouts keep the stock tab/status bars and name the visible tab after both the
workspace and declared session, while the private backend target remains opaque. A
setup failure before the client starts reopens the same workspace/session row. Once
a client actually runs and returns—normally, nonzero, by signal, or after forced
disconnection—Portagenty prints `pa ← returned from "my-workspace / shell"` and then
any abnormal diagnostics. No hand-back line is printed for dry runs or pre-client
setup failures. There are no workspace-TOML enforcement fields.

## `pa resources`

Inspect or control only workloads Portagenty can revalidate from an exact
machine-local ownership receipt.

```sh
pa resources capabilities
pa resources status
pa resources status claude
pa resources cleanup claude
pa resources stop claude
pa resources kill claude --force
```

| Command | Behavior |
|---|---|
| `capabilities` | Reports backend, available metrics/actions, all five resource-limit kinds, and degraded or unsupported reasons. |
| `status [session]` | Shows pending (active, dead-cleanable, or ambiguous), `owned`, `legacy-restart-required`, `split-containment`, `ambiguous-binding`, or `stale-binding` plus exact evidence, applied limits, and available/unavailable metrics. With no session, reports all declared sessions. |
| `cleanup <session>` | Signal-free cleanup only. Removes a pending journal when its creator, exact unit, and private target are proven gone and its marker is either absent or exactly validated with a dead recorded anchor; removes a receipt after exact unit and target absence is revalidated. Exact runtime/path/nonce shape is checked first; absent runtime components succeed without being recreated, while existing components retain owner/mode/type/protocol/nonce/PID/start-time checks. Refuses partial, live, mismatched, or probe-error evidence. |
| `stop <session>` | For owned-and-verified v2 containment only: revalidates ownership, requests graceful shutdown of the exact private multiplexer target, then performs a non-force systemd stop if needed. Never silently escalates to SIGKILL. |
| `kill <session> --force` | For owned-and-verified v2 containment only: separately explicit whole-cgroup SIGKILL after immediate ownership revalidation. Refuses without `--force`. |

Metrics include CPU totals/rate, cgroup-charged memory current/peak/events, swap
current/peak/events, task/thread current/peak/events, aggregate I/O totals/rates,
CPU/memory/I/O PSI, and cgroup state where the kernel exposes them. CPU rate may
exceed 100% on multicore workloads. Event warnings surface deltas such as memory
high/OOM/OOM-kill, task-limit hits, and CPU quota throttling. Portagenty keeps no
resource history, inspects no terminal or log content, and runs no telemetry
daemon.

## `pa claim [session]`

"Make this device the main session." Short-form alias for
takeover-attach. Session name defaults to the first one declared in
the workspace.

| Flag | Default | What |
|---|---|---|
| `-w`, `--workspace <path>` | walk-up | Explicit workspace file |
| `--dry-run` | off | Print what would happen |
| `--resume` | off | Same semantics as `pa launch --resume`: appends `--continue` for `kind = "claude-code"` sessions, one-line hint for other kinds. |
| `--fresh` | off | Same semantics as `pa launch --fresh`: kill any existing mpx session first. The zellij takeover workaround (loses running state). |

Examples:

```sh
pa claim                  # first session in workspace
pa claim tests            # specific session
pa claim --dry-run        # peek without touching
pa claim claude --resume  # takeover + resume the Claude conversation
```

## `pa list`

Print the resolved workspace (name, multiplexer, projects,
sessions) to stdout. Handy for scripts + sanity checks.

```sh
pa list
pa list -w ~/code/my.portagenty.toml
```

Example output:

```
workspace: My stuff
file:      /home/u/code/my.portagenty.toml
mpx:       Tmux
projects:  2
  - /home/u/code/one
  - /home/u/code/two
sessions:  2
  - claude  (cwd: /home/u/code/one)  claude
  - dev     (cwd: /home/u/code/two)  bun run dev
```

## `pa init [name]`

Scaffold a new `<name>.portagenty.toml` in the current directory
with one starter session (`shell` → `bash`). Designed for phone-over-
SSH: you don't have to hand-edit TOML before `pa` works.

| Flag | Default | What |
|---|---|---|
| `name` (positional) | current-directory name | Workspace display name; filename stem is a sanitized version |
| `--mpx tmux\|zellij` | global default, else tmux | Which multiplexer to pin |
| `--force` | off | Overwrite an existing `<name>.portagenty.toml` |
| `--with-agent-hooks` | off | Also scaffold `.mcp.json` + `.claude/commands/` + `.claude/skills/` so a Claude Code agent in this workspace self-discovers portaconv (conversation extractor) and the portagenty workspace shape |

```sh
pa init                        # name taken from current dir
pa init my-space               # explicit name
pa init my-space --mpx zellij
pa init my-space --force       # overwrite existing
pa init --with-agent-hooks     # also drop .mcp.json + .claude/ hooks
```

### `--with-agent-hooks`

Writes four files (skipped if already present — opt-in, not
authoritative):

- `.mcp.json` — registers the `portaconv` MCP server so Claude
  Code's MCP client can call `list_conversations` /
  `get_conversation` against this workspace's history.
- `.claude/commands/convos.md` — a slash command that lists /
  dumps conversations via `pa convos`.
- `.claude/skills/portaconv.md` — a skill describing what
  portaconv is and when to reach for it.
- `.claude/skills/portagenty-workspace.md` — a skill describing
  the workspace's TOML contract (sessions, id, `previous_paths`).

If `pconv` (portaconv) isn't on PATH yet, the hooks are still
written — they're harmless without it, and the MCP handshake starts
succeeding the moment you run `cargo install portaconv`. A hint is
printed either way.

**Safe on existing workspaces.** `pa init --with-agent-hooks` in a
directory that already has a `*.portagenty.toml` does NOT replace
the TOML; it leaves the workspace file alone and just retrofits
the agent hooks. Re-running is idempotent (skipped files stay
untouched). Pair with `--force` only when you actually want to
rewrite the TOML itself.

## `pa add <session> -c <command>`

Append a new session to the current workspace file. Faster than
editing TOML manually, especially from a phone keyboard.

| Flag | Default | What |
|---|---|---|
| `name` (positional) | — (required) | New session's name |
| `-c`, `--command <cmd>` | — (required) | Command to run |
| `--cwd <path>` | `.` | Working directory |
| `--kind <...>` | none | `claude-code`, `opencode`, `editor`, `dev-server`, `shell`, or `other` |
| `--description <text>` | none | Human note shown dimmed in the TUI |
| `-w`, `--workspace <path>` | walk-up | Explicit workspace file |

```sh
pa add claude -c "claude --resume" --kind claude-code --description "main agent"
pa add dev -c "bun run dev" --cwd ./app --kind dev-server
pa add tests -c "cargo nextest run"
```

The append preserves any comments / formatting in the existing
workspace file — we just tack on a new `[[session]]` block at the
end. Duplicate names error cleanly.

## `pa rm <session>`

Delete a session from the current workspace file. Comments and
formatting elsewhere in the file are preserved — only the matching
`[[session]]` block is excised.

| Flag | Default | What |
|---|---|---|
| `name` (positional) | — (required) | Session to remove |
| `-w`, `--workspace <path>` | walk-up | Explicit workspace file |

```sh
pa rm claude
pa rm tests -w ~/code/my.portagenty.toml
```

## `pa edit <session>`

Change one field on an existing session without opening an editor.
Pass exactly one **field** change flag (`--command` / `--cwd` /
`--kind` / `--rename` / `--description`); `--env` / `--unset-env`
are repeatable and stack freely alongside one field flag. Passing
zero changes, or more than one field flag, errors with guidance.

| Flag | What |
|---|---|
| `name` (positional) | Session to edit |
| `--command <cmd>` | Replace the command |
| `--cwd <path>` | Replace the cwd |
| `--kind <...>` | Replace the kind hint |
| `--rename <new-name>` | Rename (errors on collision with an existing session) |
| `--description <text>` | Set the description note (empty string clears it) |
| `--env KEY=VAL` | Set an env var (repeatable) |
| `--unset-env KEY` | Remove an env var (repeatable) |
| `-w`, `--workspace <path>` | Explicit workspace file (walk-up otherwise) |

```sh
pa edit claude --command "claude --resume"
pa edit dev --cwd ./new-app
pa edit my-session --kind claude-code
pa edit old-name --rename new-name
pa edit claude --description "main coding agent"
pa edit claude --description ""        # clears the note
pa edit claude --env ANTHROPIC_MODEL=opus --env DEBUG=1
pa edit claude --unset-env DEBUG
```

Same comment-preserving behavior as `pa rm`: only the target field
on the target session changes; everything else in the file is left
untouched.

## `pa convos <...>`

Workspace-aware shim over [portaconv](https://github.com/cybersader/portaconv)
(`pconv`). Forwards any subcommand + flags you pass to `pconv` with
`--workspace-toml <resolved-path>` prepended so the tool only sees
this workspace's conversation history.

| Flag | Default | What |
|---|---|---|
| `-w`, `--workspace <path>` | walk-up | Explicit workspace file (maps to pconv's `--workspace-toml`) |
| everything else | — | Passed through verbatim to `pconv` |

```sh
pa convos list                          # list conversations in this workspace
pa convos list --since 7d               # pconv flags pass through
pa convos dump <session-id>             # paste-ready markdown
pa convos dump <session-id> --rewrite wsl-to-win
```

portagenty doesn't bundle pconv. Install it separately with
`cargo install portaconv` (or drop a release binary on PATH). When
pconv isn't installed, `pa convos` exits with a clear install hint
— it doesn't silently become a no-op.

Scripts that care about pconv's exit code work the same as if you'd
invoked pconv directly: `pa convos` re-exits with pconv's status on
non-zero.

## `pa completions <shell>`

Emit a shell completion script to stdout. See
[shell completion setup](../../getting-started/completions/)
for per-shell install recipes.

```sh
pa completions bash > ~/.local/share/bash-completion/completions/pa
pa completions zsh  > ~/.zsh/completions/_pa
pa completions fish > ~/.config/fish/completions/pa.fish
```

Covers subcommand names + flag names + flag values that come from a
closed set. Dynamic completion of session names / snippet names /
workspace files is roadmapped, not v1.x.

## `pa export`

Render the resolved workspace as a multiplexer-native starter
artifact. Useful for committing alongside the workspace TOML so
teammates can launch the whole stack without installing `pa`
themselves.

| Flag | Default | What |
|---|---|---|
| `-w`, `--workspace <path>` | walk-up | Explicit workspace file |
| `--format tmux\|zellij` | workspace's `multiplexer` | Output format |
| `-o`, `--output <path>` | stdout | File to write to |

Examples:

```sh
pa export                             # stdout, format auto-picked
pa export --format zellij             # zellij KDL layout
pa export --format tmux -o starter.sh # save to file
```

Outputs a POSIX shell script for `--format tmux` (`tmux
new-session -d` per session + `tmux attach-session -d` to the
first) or a KDL layout with one tab per session for `--format
zellij`. Both respect env vars declared on sessions and sanitize
session names the same way `pa` does at runtime.

## `pa onboard`

Re-run the first-run wizard at any time. Interactive; walks you
through workspace scaffolding, multiplexer choice (with installed /
not-found annotations), optional Claude Code starter session, and
offers to set or change the machine-default multiplexer. Writes a
`<name>.portagenty.toml` in the current directory and auto-registers
it in the global workspace index so `pa` from anywhere can find it.

```sh
pa onboard
```

No flags — the wizard is fully interactive. Safe to re-run: an
existing workspace file in the current directory is left untouched.

## `pa snippets`

Bundled bash ergonomics shipped inside the `pa` binary. Idempotent:
installing twice replaces the block in-place via a marker comment so
your rc file never accumulates duplicates.

The `pa-aliases` snippet includes a `paclaim` shell function for
claiming a session **from inside** it (once pa has handed off to
tmux/zellij, `pa claim` is refused due to nested-mpx guard). On
tmux, `paclaim` runs `tmux detach-client -a` — kicks all other
clients but keeps your current terminal attached. On zellij, there's
no built-in equivalent; the function prints the detach + reattach
workaround.

### `pa snippets list`

Print the bundled snippet catalog with one-line descriptions.

```sh
pa snippets list
```

### `pa snippets show <name>`

Print a snippet's contents to stdout. Review before installing.

```sh
pa snippets show pa-aliases
```

### `pa snippets install <name>`

Install (or update) a snippet in your rc file.

| Flag | Default | What |
|---|---|---|
| `name` (positional) | — (required) | Snippet name from `pa snippets list` |
| `--to <path>` | `~/.bashrc` | Target rc file |
| `--dry-run` | off | Preview the result without writing |

```sh
pa snippets install pa-aliases
pa snippets install termux-friendly --to ~/.zshrc
pa snippets install pa-aliases --dry-run
```

### `pa snippets uninstall <name>`

Remove a previously-installed snippet from your rc file. Surrounding
user content is preserved byte-for-byte.

| Flag | Default | What |
|---|---|---|
| `name` (positional) | — (required) | Snippet name to remove |
| `--from <path>` | `~/.bashrc` | Target rc file |
| `--dry-run` | off | Preview the result without writing |

```sh
pa snippets uninstall pa-aliases
```

## `pa open <url>`

Dispatch a `pa://...` URL to the matching pa action. Called
automatically by the OS when the user clicks a `pa://` link (see
`pa protocol` below). Supported URL shapes:

| URL | Opens |
|---|---|
| `pa://open/<path>` | Workspace TUI for the workspace at `path` (percent-encoded) |
| `pa://shell/<path>` | Plain shell at `path` (no pa state, no mpx) |
| `pa://workspace/<uuid>` | Workspace whose TOML has `id = "<uuid>"` |
| `pa://launch/<uuid>/<session>` | Launch `session` in the workspace with that id |

```sh
pa open "pa://open/home/u/code/myproject"
pa open "pa://workspace/a1b2c3d4-e5f6-7890-abcd-ef1234567890"
```

Unknown actions error cleanly rather than silently opening the
picker — clicks are asynchronous and a wrong-scheme URL shouldn't
leave you staring at a generic home screen.

## `pa protocol`

Manage the OS-level `pa://` URL scheme so browser / note-app / Slack
clicks on `pa://...` links launch pa in your terminal.

### `pa protocol terminals`

List terminal emulators detected on this machine, highest-priority
first. The first entry is what `install` / `show` default to.
Detection covers: Windows Terminal, ConEmu, Alacritty, WezTerm,
cmd.exe (Windows) · GNOME Terminal, Konsole, Alacritty, Kitty,
WezTerm, Foot, XFCE Terminal, xterm (Linux) · iTerm2, Terminal.app,
Alacritty, WezTerm, Kitty (macOS).

On WSL, Windows terminals (e.g. `wt.exe`) are preferred — URL
clicks originate from Windows, so that's where the handler lives.

```sh
pa protocol terminals
```

### `pa protocol show [--terminal <name-or-path>]`

Print the OS-appropriate registration snippet without writing
anything. Safe — always a read-only preview. Output is:

- **Linux** → a `.desktop` file body
- **Windows / WSL** → a `.reg` file body
- **macOS** → guidance (install not automated there)

```sh
pa protocol show
pa protocol show --terminal alacritty       # override the default
pa protocol show --terminal /opt/my-term    # absolute path works too
```

### `pa protocol install [--terminal <name-or-path>]`

Write the registration:

- **Linux** → `~/.local/share/applications/portagenty.desktop` + runs
  `xdg-mime default portagenty.desktop x-scheme-handler/pa` so the
  desktop environment picks it up immediately.
- **Windows / WSL** → `HKCU\Software\Classes\pa` (user-scope, no
  admin needed; works from WSL via `reg.exe`).
- **macOS** → errors; use `pa protocol show` and apply the Info.plist
  guidance manually.

`--terminal` accepts either a detected terminal name (substring
match, case-insensitive) or any absolute path / PATH-resolvable
binary. Custom terminals get a generic `-e {cmd}` template — works
for most POSIX emulators.

```sh
pa protocol install                          # auto-pick the best
pa protocol install --terminal alacritty
pa protocol install --terminal /usr/local/bin/my-terminal
```

### `pa protocol uninstall`

Reverse of `install`. Idempotent — removing an already-absent
registration is a no-op.

```sh
pa protocol uninstall
```

### `pa protocol status`

Report what's currently registered. Useful for verifying install or
debugging "why doesn't my pa:// link work?".

```sh
pa protocol status
```
