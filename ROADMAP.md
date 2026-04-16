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

1. **zellij adapter.** List/has/kill via imperative CLI; create +
   attach via generated KDL layout files. Inside-zellij detection
   returns a clear "detach first" error instead of the opaque
   nesting failure. 7 e2e tests against real zellij on Linux CI.
3. **Untracked session adoption.** The TUI merges `mux.list_sessions`
   with workspace definitions and surfaces three row states — Live,
   NotStarted, Untracked — each with a distinct color marker.
   Enter routes to `attach` for Live/Untracked and `create_and_attach`
   for NotStarted.
6. **State/activity decorations.** Delivered alongside untracked
   adoption: ● (green) live, ○ (dim) idle, ? (yellow) untracked,
   plus a `[label]` tag on each row.
7. **Declarative export (`pa export`).** Renders the resolved
   workspace as a POSIX shell script (`tmux new-session -d` per
   session + `tmux attach -d`) or as a zellij KDL layout (one tab
   per session, env routed through `command "env"` for clean string
   args). Commit a starter script alongside the workspace TOML and
   teammates can launch the whole stack without installing `pa`.
9. **`kind:` session hints.** Optional `kind` field on sessions
   (claude-code / opencode / editor / dev-server / shell / other).
   Currently display-only — the TUI shows a one-letter color-coded
   glyph (C / O / E / D) next to the state marker.
10a. **Per-session env vars.** `env` field on sessions threaded
   through the merge into the launch path. tmux uses `-e KEY=VAL`
   per entry; zellij wraps `bash -c "<cmd>"` in `env KEY=VAL ...`
   so values with spaces / symbols don't need shell escaping.
   Pre/post commands + profile refs (item 10's other halves) still
   to ship.

Plus several **unplanned wins** that dropped out of dogfooding on
real projects:

- **`pa claim` cross-device takeover.** Solves the "screen size
  stuck after attaching from a smaller device" issue inherent to
  multi-client tmux sessions. tmux uses `-d` natively; zellij warns
  when other clients appear attached (no equivalent CLI).
- **Full no-editor session lifecycle.** `pa init / add / rm / edit`
  with comment-preserving toml_edit writes. Makes phone-over-SSH
  authoring practical.
- **Onboarding wizard** (`pa onboard`). Progressive 30-second first-
  run flow: pick workspace name, multiplexer (with install-detection
  badges), Claude-Code starter session. Auto-registers the new
  workspace in the global index.
- **Workspace picker "home screen"** in the TUI. When `pa` runs
  outside any walk-up tree, a ratatui picker lists registered
  workspaces + a "live sessions on this machine" sentinel. Android-
  back navigation: Esc from session TUI always returns to the
  picker; q / Ctrl+C exits.
- **Workspace recency** across the picker (sort + "X ago" column)
  and session list (LAST column on Live rows at ≥80 cols). Reads
  from the state store written since v1.
- **Bundled bash snippets** (`pa snippets install pa-aliases` /
  `termux-friendly`). Idempotent marker-block install into `~/.bashrc`
  or equivalent.
- **Shell completion** (`pa completions bash|zsh|fish|elvish|powershell`).
  Static subcommand + flag + closed-enum-value completion.
- **`pa launch --resume` / `pa claim --resume`.** For sessions with
  `kind = "claude-code"`, appends `--continue` before launch. Other
  kinds get a one-line hint. Workspace TOML stays literal.
- **Visual differentiation pass.** Title shows an mpx badge
  (cyan `[tmux]` / magenta `[zellij]`). Session-name color matches
  state (green/dim/yellow). Live rows show attached-client count
  when tmux reports it (`[live · 2 clients]`).
- **Pre-launch banner.** Just before handing off to the mpx,
  prints `pa → <mpx> session "<name>"  ·  detach: <chord>  ·
  re-attach: pa claim <name>` so the user sees the exit path on
  their actual shell.
- **In-TUI find-folder + scaffold (`n` in picker).** Press `n`
  in the workspace picker to open a search overlay. Tiered
  candidate sources: recents (LRU from state.toml), zoxide
  (frecency, if installed), plocate / locate / Everything CLI
  (pre-built indexes), fd (live walk respecting .gitignore),
  stdlib walker (always-available fallback, depth-capped, with
  a hardcoded ignore list for .git / node_modules / target /
  __pycache__ / .venv / dist / build). Results merged + deduped
  on canonical path, then ranked by `nucleo` (Helix's pure-Rust
  fuzzy matcher). Enter on a candidate either opens an existing
  workspace there or pops a confirm to scaffold one — on
  confirm, `crate::scaffold::create_at` writes the TOML, registers
  globally, and the picker exits with the new workspace as its
  outcome so the session TUI opens immediately.
- **Responsive (key, label) footer.** Each TUI screen builds a
  prioritized list of `(key, label)` pairs; the renderer drops
  least-important entries first to fit the width, then drops
  labels for keys-only mode if needed. Mobile (<30 col) Termux
  always sees `?` and `q` plus their labels.
- **Picker row actions.** `d` unregister, `D` delete file with
  confirm, `r` reveal path in a sticky info modal with auto
  copy-to-clipboard.
- **Workspace scaffolder extracted.** `crate::scaffold::create_at`
  is the single source of truth for filename sanitization, TOML
  body rendering, and global registration. `pa init`, the
  onboarding wizard, and the in-TUI find/scaffold flow all
  delegate to it.

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
   into the resolved `Workspace`. Design intent: tags are a
   picker-level perspective (group registered workspaces by project
   tags), not a session-level filter.
5. **Custom ordered groups.** Hand-curated named groups
   ("playlists"). Drag-in-TUI or edit via keybinding.
5a. **Datetime-out-front in the session explorer.** Promote the
   relative-time hint ("2h ago") to an absolute wall-clock column
   visible near the left of each live row (e.g. "Tue 14:32" or
   "2026-04-15 14:32" depending on width). Makes it obvious which
   session you were *just* in versus one from last week without
   doing mental math. The `state::last_launch_for_session` lookup
   and `relative_time` formatter are already in place; this is
   purely a render-layer change plus a width-tier decision.
8. **Eager / "jump-in" launch.** `pa up` / `--eager` flag to
   pre-spawn all workspace sessions in detached mode (fully
   supported on tmux; partial on zellij due to the
   no-background-with-command limitation).
10b. **Session schema extensions, part 2.** Pre/post commands +
    profile references. Schema scaffolding from part 1 (env)
    extends naturally.
11. ~~**`fd` / Everything CLI search integration.**~~ **Shipped.**
    Landed as part of the in-TUI find-folder flow's tiered backend
    (see Shipped section above). Tier order: recents → zoxide →
    plocate/locate/Everything → fd → stdlib walker. Each tier is
    silently skipped when its tool isn't on PATH.
12. **Termux polish pass.** Over-SSH verification + any rough
    edges found in real use. Core keybindings are already
    Termux-safe by default (`DESIGN.md` §10).
13. **"Jump back to pa" from inside a running session.** Currently
    once `pa` launches you into a zellij/tmux session, `pa` itself
    exits — the multiplexer owns the terminal. There's no way to
    jump back to the picker or session list without detaching from
    the multiplexer first (`Ctrl+O d` in zellij, `Ctrl+B d` in
    tmux) and then re-running `pa`. This is the #1 UX confusion
    reported during Termux mobile testing.

    **The problem in detail**: `pa`'s current lifecycle is
    "show TUI → user picks a session → restore terminal → exec
    the multiplexer attach command → `pa` process exits." After
    that, `pa` is gone from the process tree. The multiplexer is
    in charge. Typing `pa` inside the session hits the nested-mpx
    pre-flight check and refuses with an error. The user is stuck
    until they remember the detach chord.

    **Solution options explored (ranked by feasibility)**:

    **(A) Relax the nested-mpx check — read-only picker inside a
    session.** Instead of refusing when `pa` is run inside zellij/
    tmux, open a lightweight read-only TUI that shows the workspace
    picker and session list. The user can browse, see what's running,
    and the footer says "detach: Ctrl+O d to switch sessions" with
    the correct chord prominently displayed. On Enter, instead of
    trying to attach (which would nest), print the detach
    instructions or, for tmux, run `tmux switch-client -t <target>`
    (tmux supports switching between sessions from inside a client
    — zellij does not). **Effort**: medium. Requires a new
    `--nested` mode in the TUI that skips the attach path and
    replaces it with informational output. Tmux gets real
    switching; zellij gets a "here's how to detach" hint.

    **(B) Tab-based session model.** Instead of creating each
    workspace session as a separate zellij/tmux session, create
    them as **tabs within one multiplexer session.** The user
    switches between Claude Code and shell via Alt+1 / Alt+2
    (zellij tab switching) or Ctrl+B 1 / Ctrl+B 2 (tmux window
    switching). `pa` could be a permanent first tab — the "home
    screen" the user always returns to. Switching tabs is instant,
    no detach/reattach needed, no chord to remember.
    **Effort**: large. Requires rearchitecting the launch path
    to use `zellij action new-tab` / `tmux new-window` instead of
    `zellij attach --create` / `tmux new-session`. The
    cross-device takeover story changes too (you'd take over the
    whole multiplexer session, not individual sub-sessions).
    Trade-off: simpler in-session switching, but the workspace-
    session model becomes multiplexer-window-based, which may
    conflict with users who already have their own tab layouts.

    **(C) `pa` as a persistent background process.** Instead of
    exiting after launch, `pa` stays resident and listens for a
    keybind (e.g. a global tmux/zellij keybind that sends a signal
    or runs `pa --show`). When triggered, `pa` pops the TUI
    overlay on top of whatever is running, like a quake-style
    dropdown terminal. **Effort**: very large. Requires `pa` to
    manage its own lifecycle, IPC, and terminal multiplexing —
    essentially becoming a mini window manager. Conflicts with
    DESIGN.md's anti-daemon stance (§5). Not recommended unless
    the simpler options prove insufficient.

    **(D) Multiplexer keybind snippet.** Ship a zellij/tmux config
    snippet (via `pa snippets install zellij-jump-back`) that
    binds a key (e.g. Ctrl+P or a custom chord) to: detach from
    current session, then run `pa` in the resulting shell. This is
    a two-step operation collapsed into one keypress via the mpx
    config. **Effort**: small, but it's a config snippet, not a
    code feature — lives in the user's mpx config and is
    fragile across mpx version updates. Best as a complement to
    option (A), not a replacement.

    **Recommended path**: ship **(A)** first (read-only picker
    inside a session, with tmux switch-client for real tab
    switching), then evaluate whether **(B)** is worth the
    architecture change based on real usage. **(D)** can ship
    alongside (A) as an ergonomic bonus. **(C)** is deferred
    unless demand justifies the complexity.

14. **Session-name namespacing (collision bug).** All workspaces
    scaffold sessions with generic names like `"shell"` and
    `"claude"`. The multiplexer doesn't namespace them — `zellij
    attach --create shell` attaches to whichever `shell` session
    exists, regardless of which workspace created it. If you have
    cyberchaste and dataflowy both with a `"shell"` session, opening
    cyberchaste's shell might land you in dataflowy's session (or
    vice versa) because the mpx sees them as the same name.

    **The problem**: `mux::sanitize_session_name` produces the mpx
    session name from the session's declared `name` field. Two
    workspaces with `name = "shell"` produce the same mpx name
    `"shell"`. There's no workspace prefix, no hash suffix, no
    disambiguation.

    **Fix options**:

    **(A) Prefix with workspace name.** Sanitized mpx name becomes
    `<workspace>-<session>`, e.g. `cyberchaste-shell`,
    `dataflowy-shell`. Simple, readable in `tmux list-sessions` /
    `zellij list-sessions`. Breaks existing sessions (they'd need
    to be killed and recreated). The TUI's session-list display
    name stays unprefixed (user sees "shell", mpx sees
    "cyberchaste-shell").

    **(B) Hash suffix.** Append a short hash of the workspace file
    path to the session name: `shell-a3f2`. Less readable but
    guarantees uniqueness even across workspaces with the same name.

    **(C) Workspace-scoped mpx sessions.** Use tmux's `-L` /
    zellij's `--session` to scope sessions per workspace. Each
    workspace gets its own tmux socket / zellij session group.
    Clean isolation but makes `pa list` and untracked adoption
    more complex (need to probe multiple sockets).

    **Recommended**: **(A)** — simple, solves 99% of collisions,
    human-readable. Migrate existing sessions by killing and
    re-creating on first launch after the change.

15. **CWD edit should use the find/tree browser.** The `e → c`
    (edit cwd) flow currently asks for a raw text path via a tiny
    input box. This is confusing — editing a cwd is fundamentally
    "point me at a folder," which is what the find overlay and tree
    browser were built for. The edit-cwd flow should open the same
    find/tree interface, with the current cwd as the starting root,
    and on selection write the chosen path back to the session's
    `cwd` field. Same UX as the `n` (new workspace) flow but
    targeting an existing session's cwd instead of scaffolding.

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
