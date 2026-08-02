# Security policy

## Reporting a vulnerability

Prefer a private report:

- GitHub: [open a Security Advisory](https://github.com/cybersader/portagenty/security/advisories/new)
- Email: l3.aitools@inbox32.com

Please include:

- What version / commit you reproduced on.
- Steps to reproduce, or a minimal PoC.
- What you consider the realistic impact.

Please do not open a public issue for a suspected vulnerability.

## Scope

portagenty is a local, terminal-native launcher. It shells out to
multiplexers (tmux, zellij, WezTerm) and to agent CLIs (Claude Code,
OpenCode, etc.) but does not itself open network sockets, accept
inbound connections, or transmit data off the machine.

In scope:

- Shell injection through workspace / project / session config fields.
- TOML parsing that causes crashes, DoS, or unsafe deserialization.
- Tmux / zellij / WezTerm command construction that could execute
  unintended commands.
- Path handling issues (traversal, symlink races) when walking to find
  workspace files.
- Any bug that causes `pa` to take a destructive action on user files
  without explicit intent.

Out of scope:

- Vulnerabilities in upstream dependencies (ratatui, crossterm, tmux,
  etc.). Please report those upstream; we track them via Dependabot and
  update when a fix is reachable (see "Current known issues" below).
- Threats that assume arbitrary code execution on the user's machine
  (portagenty inherits the user's permissions).
- Issues specific to an agent CLI launched via a session (those are
  the agent's concern, not portagenty's).

## Current known issues

Dependabot has open alerts against this repo. None are believed to be
exploitable in how portagenty is built and used, and each is accepted for a
specific reason rather than dismissed. Reassess whenever the surrounding
facts change — particularly if the docs site ever gains a server renderer.

### `lru` (low) — via `ratatui`, in the shipped binary

`IterMut` violates Stacked Borrows by invalidating an internal pointer. This
is a soundness lint (Miri-detected UB), not a remote or input-driven vector,
and `lru` is reachable only through `ratatui`'s internal rendering cache.

**Not fixable here.** `ratatui 0.29` pins `lru 0.12.x`; the fix landed in
`0.16.3`. `cargo update -p lru` is a no-op — verified. This unblocks when
ratatui widens its constraint, not before.

### `astro` (2 moderate, 1 low) — docs site only

Three XSS advisories: unescaped spread attribute names, unescaped
`transition:*` directive values, and unescaped View Transition animation
properties.

**Not exploitable in this configuration.** `docs/` builds as a static site —
no adapter, no `output: "server"` — so pages are pre-rendered HTML published
to GitHub Pages. All three advisories require a server rendering
attacker-controlled input at request time. There is no request time.

The fixes require Astro 7.x (currently on 6.1.6), a major upgrade that pulls
`@astrojs/starlight`, the MDX pipeline, and the theme plugins with it. That
is a deliberate docs migration, not a patch, and is tracked as its own task.

**This rationale expires** if the docs ever adopt an SSR adapter or hybrid
rendering. At that point these become live issues and the upgrade is
required, not optional.

### `sharp` (high) — build-time only

Inherited libvips CVEs (CVE-2026-33327, -33328, -35590, -35591).

`sharp` is Astro's *optional* image-optimization dependency. It runs at build
time, on this repo's own images, on a developer machine or CI runner — it is
not part of the published site and never processes untrusted input. Astro 6
declares `sharp ^0.34.0`, so pinning the patched `0.35.0` in `docs/package.json`
would fight the resolver to fix something that only ever sees our own files.
Resolves naturally with the Astro 7 upgrade.

### Reassessment triggers

- The docs site gains SSR / hybrid rendering, or starts rendering
  user-supplied content → the Astro advisories become live.
- `ratatui` widens its `lru` constraint → bump immediately.
- Image processing starts handling untrusted uploads → `sharp` becomes live.
