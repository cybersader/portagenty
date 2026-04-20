# Proposal — `pa convos`: Paste-First Conversation Extractor

A design and problem statement for letting portagenty **extract, render,
and hand off** conversation context from any agentic coding tool, so the
user can paste it into a new session on any surface.

> **Status:** Draft. Pivoted from a sync/bridge approach after learning
> that conversation JSONL content contains OS-specific absolute paths that
> cannot be meaningfully sync'd — only translated or discarded.
>
> **Companion to:** [DESIGN.md §11](./DESIGN.md) "Agent context persistence and cross-environment sync"
>
> **Related external tool:** `tools/claudecode-project-sync/` in [agentic-workflow-and-tech-stack](https://github.com/cybersader/agentic-workflow-and-tech-stack) ([also](https://github.com/cybersader/cybersader-agentic-workflow-and-tech-stack)) — the sync approach this proposal steps away from.
>
> **Origin:** Real-user pain encountered on 2026-04-19 — see [Recovery Story](#6-recovery-story-2026-04-19) below.

---

## 1. Why the sync approach was a dead end

### 1a. The surface problem: WSL vs Windows path encoding

Claude Code indexes conversation history by the **absolute filesystem
path of the working directory at launch time**, encoded into a directory
name under `~/.claude/projects/`. Same project, two buckets:

| Launch surface | CWD Claude Code sees | Encoded directory |
|----------------|----------------------|-------------------|
| WSL bash | `/mnt/c/Users/Cybersader/Documents/…/proj` | `-mnt-c-Users-Cybersader-Documents-…-proj` |
| PowerShell / Windows | `C:\Users\Cybersader\Documents\…\proj` | `C--Users-Cybersader-Documents-…-proj` |

### 1b. The deep problem: paths are baked into conversation content

Spot-check of one real 54 MB session JSONL in the Windows-encoded bucket:

- **9999+ occurrences of `/mnt/c/…`** absolute paths (WSL-authored)
- **72 occurrences of `C:\…`** absolute paths (Windows-authored)
- Embedded inside `cwd` fields, `file_path` on Read/Edit/Glob tool calls,
  and prose references throughout the dialogue

### 1c. Why this kills every file-level sync strategy

Merging the two encoded directories — by symlink, copy, bind-mount, or
Docker daemon — does not solve the problem. It only merges storage. The
content inside still carries OS-specific path references. A Windows-launched
Claude Code resuming a WSL-authored conversation fails the first time it
tries to Read `/mnt/c/…`. And vice versa.

Existing user workaround `tools/claudecode-project-sync/`
([README](https://github.com/cybersader/agentic-workflow-and-tech-stack))
has a Docker mode polling every 15 s + a symlink mode. Both operate at
the file layer. The [recovery story](#6-recovery-story-2026-04-19) shows
them silently diverging in practice.

### 1d. Upstream tracking (for completeness, not dependency)

- [anthropics/claude-code#17682](https://github.com/anthropics/claude-code/issues/17682) — Cross-Environment Conversation History Synchronization
- [anthropics/claude-code#9668](https://github.com/anthropics/claude-code/issues/9668) — Duplicate 'Warmup' titles / wrong conversations in WSL
- [anthropics/claude-code#9306](https://github.com/anthropics/claude-code/issues/9306) — Project-Local Conversation History Storage

**We do not build on the assumption these land.**

---

## 2. The pivot — paste-first recovery

> User's stated preference:
> "Since I don't move between platforms a lot, I say we make the maybe
> expensive but more practical straightforward decision of having a system
> that will take your context and you can just paste it. Maybe I design
> this into portagenty and I allow it to plug into all sorts of different
> agentic tool conversation formats and conventions."

This flips the problem:

- **Don't** teach Claude Code to see across encodings.
- **Don't** run a daemon that syncs files.
- **Do** extract the substantive dialogue from any tool's storage format,
  render it as paste-ready text, and let the human (or another agent)
  drop it into a fresh session wherever that session needs to live.

The fallback becomes the first-class feature. It is strictly more robust
because:

- A new chat has no embedded paths → no OS-specific failure mode.
- It works across Claude Code, claude.ai web, Claude API, **and**
  other tools entirely (OpenCode, Cursor, Aider, etc.).
- It composes with any future upstream fix — it doesn't compete with it.
- It's on-demand. No daemon. No race windows. No silent divergence.

Cost: you lose tool-call state on resume. The new session starts with
prose context, not a live replay of prior tool invocations. For most
recovery scenarios this is fine — the substantive reasoning is what
carries over; the tool calls are re-runnable against the current file
system state anyway.

---

## 3. `pa convos` — design

### 3a. CLI surface

```sh
# List conversations discoverable from this workspace's scope
pa convos
pa convos list [--tool claude-code|opencode|cursor|aider|...]
               [--since 2d] [--workspace <id>]

# Dump a specific session to stdout (paste-ready markdown)
pa convos dump <session-id>
               [--format markdown|xml|json|plain]
               [--rewrite-paths wsl-to-win|win-to-wsl|strip]
               [--tail <N-messages>] [--max-tokens <N>]
               [--include tool-calls|tool-results|prose-only]

# Copy to system clipboard (calls into OS clipboard, doesn't sync)
pa convos copy <session-id> [same flags as dump]

# Export to a file — useful for committing context alongside a workspace
pa convos export <session-id> --to ./HANDOFF.md [same flags]

# Helper: show which tools/formats are auto-detected
pa convos adapters
```

No subcommand makes writes back to the tool's own storage. **Read-only.**

### 3b. Adapter model — pluggable per-tool

Each agentic tool has its own conversation storage format. The adapter
trait is the project's contract:

```rust
// src/convos/adapter.rs (sketch)

/// A read-only adapter for one agentic tool's conversation storage.
pub trait ConvoAdapter {
    /// Short stable identifier (e.g., "claude-code", "opencode").
    fn id(&self) -> &'static str;

    /// Detect whether this adapter applies on this machine
    /// (e.g., ~/.claude/projects/ exists for claude-code).
    fn detect(&self) -> bool;

    /// List sessions visible to this adapter, optionally scoped
    /// to workspace paths. Returns abstract SessionMeta entries.
    fn list(&self, scope: Option<&WorkspaceScope>) -> Result<Vec<SessionMeta>>;

    /// Load a full session into the shared in-memory model.
    fn load(&self, id: &SessionId) -> Result<Conversation>;
}
```

The shared `Conversation` model is tool-agnostic: ordered messages,
tool-call + tool-result pairs, optional metadata. Adapters normalize
their native format into this model on load. The renderer operates
only on the shared model.

Initial adapter targets (v1 of `pa convos`):

| Adapter | Storage | Priority | Notes |
|---------|---------|----------|-------|
| `claude-code` | `~/.claude/projects/*/*.jsonl` | **P0** | Drives the user's current pain |
| `opencode` | TBD | P1 | Other half of the scaffold's tool-pair |
| `cursor` | Cursor's chat export | P2 | JSON from Cursor's chat UI |
| `aider` | `.aider.chat.history.md` | P2 | Markdown, easy |
| `continue.dev` | `.continue/sessions/` | P3 | JSON sessions |

The adapter contract makes each of these a small self-contained module.
Users can contribute new adapters without touching the core.

### 3c. Rendering

The renderer takes a `Conversation` + flags and produces output:

- **markdown (default)** — human-readable, great for pasting into any
  chat UI. Tool calls rendered as fenced code blocks with tool name +
  inputs. Tool results collapsed to summaries by default.
- **xml** — for tools/prompts that prefer structured tags.
- **json** — passthrough for machine processing.
- **plain** — strip all tool calls, keep only user/assistant prose.

Shared optional transforms (all opt-in):

- **Path rewriting** — `/mnt/c/X` ↔ `C:\X`, `--strip` to remove paths
  entirely. Addresses the content-layer problem from §1b.
- **Trimming** — `--tail N`, `--since <when>`, `--max-tokens N`.
- **Redaction** — regex-based, for secrets that leaked into the
  dialogue.

### 3d. Workspace integration

`pa` already knows:

- The workspace TOML's declared projects and paths.
- The workspace `id` anchor ([DESIGN §11](./DESIGN.md)).

Use both to scope conversation discovery:

```sh
# From inside a workspace, defaults to workspace scope
pa convos list
# → Only shows conversations whose CWD falls under workspace's project paths

# Override with --all or explicit workspace
pa convos list --all
pa convos list --workspace abc-123-def
```

This is the feature the existing viewers don't have: the rest of them
show "all projects" as a flat list. `pa convos` shows "all conversations
that belong to *this* workspace" regardless of the OS encoding of the
storage directory.

### 3e. Committable manifests (optional, later)

Once you've rendered a conversation you care about, commit it:

```sh
pa convos export <id> --to docs/agent-context/2026-04-19-recovery.md
git add docs/agent-context/2026-04-19-recovery.md
git commit -m "Capture recovery context for session X"
```

The workspace is now the authoritative record of the conversation slice
that mattered. The JSONL can be deleted by Claude Code's retention, but
the important context travels with the repo.

A `pa convos manifest` command that produces an index of `(session-id,
workspace-id, summary, paths-touched)` would be strictly additive and
nice to have, but v1 doesn't need it.

---

## 4. Module layout in portagenty source

```
src/
├── convos/                       # NEW
│   ├── mod.rs                    # trait, shared types, CLI entry points
│   ├── model.rs                  # Conversation, Message, ToolCall, etc.
│   ├── render/
│   │   ├── mod.rs
│   │   ├── markdown.rs
│   │   ├── xml.rs
│   │   └── plain.rs
│   ├── transform/
│   │   ├── paths.rs              # WSL ↔ Windows rewrites
│   │   ├── trim.rs
│   │   └── redact.rs
│   └── adapters/
│       ├── mod.rs
│       ├── claude_code.rs        # P0
│       ├── opencode.rs           # P1
│       ├── cursor.rs             # P2
│       └── aider.rs              # P2
├── export/                       # existing — tmux/zellij workspace render
├── domain/                       # Workspace, Session, …
└── cli/
    └── mod.rs                    # adds `convos` subcommand tree
```

Nothing else in the codebase needs to know `convos` exists until someone
runs `pa convos`. It's additive.

---

## 5. Non-goals

- **Not a viewer.** Use [claude-code-viewer](https://github.com/d-kimuson/claude-code-viewer) or [raine/claude-history](https://github.com/raine/claude-history) for interactive browsing; if someone wants a TUI picker on top of `pa convos list`, that's v2.
- **Not a syncer.** No daemon, no polling, no writes to any tool's storage.
- **Not a patch to Claude Code's internals.** We produce paste-ready text and stop.
- **Not cross-machine transport.** Paired with git, yes, but `pa convos` itself just reads local.
- **Not a replacement for upstream (#17682).** When Anthropic fixes the index, `pa convos` is still useful as the cross-tool / paste-ready feature.

---

## 6. Recovery Story (2026-04-19)

Concrete record of what prompted this proposal:

- User was running `claudecode-project-sync` Docker container to bridge
  WSL/Windows Claude Code conversation dirs.
- `/resume` from WSL started showing conversations "from 2 months ago"
  despite an active conversation the previous night.
- Investigation found **six** encoded directories for one project:
  - `-mnt-c-…mcp-workflow-and-tech-stack` (54.1 MB, `cybersader` owned, today)
  - `C--…mcp-workflow-and-tech-stack` (54.1 MB, `root` owned, today, ~20 KB diverged)
  - `-mnt-c-…mcp-workflow-and-tech-stack-tools-terminal-workspaces` (8.4 MB, March)
  - `C--…-tools-terminal-workspaces` (8.4 MB, March)
  - `-mnt-c-…mcp-workflow-and-tech-stack-ultimate-workflow` (empty)
  - `C--…-ultimate-workflow` (empty)
- User reported "getting the issue in all my Claude projects."
- Inspection of one 54 MB JSONL revealed 9999+ `/mnt/c/` and 72 `C:\`
  path references baked into the content — the content-layer finding
  that killed the sync approach and birthed this proposal.

---

## 7. Open questions

1. **Adapter discovery.** Auto-detect or require the user to opt in to
   each tool? Start with explicit `--tool` flag, auto-detect later.

2. **Cross-tool identity.** Does portagenty attempt to dedupe "the
   same conversation continued from tool A in tool B"? Probably not in
   v1; the workspace + timestamp gives the human enough to pick.

3. **Clipboard ergonomics.** `pa convos copy` needs a clipboard
   abstraction that works on Linux (xclip/wl-clipboard), macOS
   (pbcopy), and Windows (clip.exe). WSL bridges to `clip.exe`. Solved
   problem; need to pick a crate.

4. **Tool-call rendering.** How verbose? `Read /path/X` is useful
   context; full file contents in the tool result are often just
   noise. Default to tool-name + args + a truncated-result summary
   with a `--full-results` escape.

5. **Size limits.** These JSONLs get big (54 MB today's session). The
   renderer must stream and cap output. Suggest default `--max-tokens
   50k` with `--full` to override.

6. **Deprecating `claudecode-project-sync`.** Once `pa convos` is in
   users' hands, does the Docker sync tool go away? Or stay as an
   "aggressive" option for users who want both buckets always merged?
   Recommend: mark it as "legacy, not recommended; use `pa convos`
   instead" in its README, keep it around.

---

## 8. Decision needed

- [ ] **Accept** — add to [ROADMAP.md](./ROADMAP.md), P0 adapter first
- [ ] **Refine** — tighten CLI surface or adapter trait before committing
- [ ] **Defer** — keep DESIGN §11 as-is, point users at viewers

---

## 9. References

### Internal
- [portagenty DESIGN.md §11 — Agent context persistence and cross-environment sync](./DESIGN.md)
- [portagenty ROADMAP.md](./ROADMAP.md)
- [portagenty README.md](./README.md)
- Companion challenge in the workflow repo: `research/zz-challenges/02-claude-code-conversation-fragmentation.md`

### External — upstream tracking (observed, not depended on)
- [anthropics/claude-code#17682](https://github.com/anthropics/claude-code/issues/17682) — Cross-Environment Sync feature request
- [anthropics/claude-code#9668](https://github.com/anthropics/claude-code/issues/9668) — Duplicate 'Warmup' titles in WSL
- [anthropics/claude-code#9306](https://github.com/anthropics/claude-code/issues/9306) — Project-Local Conversation History Storage

### External — existing tools worth pointing users at
- [d-kimuson/claude-code-viewer](https://github.com/d-kimuson/claude-code-viewer) — full-featured web viewer (recommended for browsing UX)
- [jhlee0409/claude-code-history-viewer](https://github.com/jhlee0409/claude-code-history-viewer) — desktop viewer
- [raine/claude-history](https://github.com/raine/claude-history) — TUI fuzzy search
- [kvsankar/claude-history](https://github.com/kvsankar/claude-history) — extract/convert by workspace
- [agsoft Claude History Viewer](https://marketplace.visualstudio.com/items?itemName=agsoft.claude-history-viewer) — VS Code sidebar

### External — sync approaches (step away from, don't build on)
- [tawanorg Claude Sync](https://medium.com/codex/sync-your-claude-code-sessions-across-all-devices-2e407c2eb160) ([DEV.to mirror](https://dev.to/tawanorg/claude-sync-sync-your-claude-code-sessions-across-all-your-devices-simplified-49bl))
- [porkchop/claude-code-sync](https://github.com/porkchop/claude-code-sync)
- [Medium — Lost a day, git-repo fixed it](https://medium.com/@creativeaininja/i-lost-a-full-day-of-claude-cowork-projects-overnight-a-git-repo-fixed-it-in-10-minutes-7742ee53046e)

### External — context
- [kentgigger — How to resume, search, and manage Claude Code conversations](https://kentgigger.com/posts/claude-code-conversation-history)
- [CodeAgentSwarm — Claude Code History Complete Guide 2026](https://www.codeagentswarm.com/en/guides/claude-code-history-complete-guide)
- [Claude Code Common Workflows docs](https://code.claude.com/docs/en/common-workflows)
