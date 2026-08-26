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

## Experimental Linux resource-supervision boundary

Opt-in supervised launches use the caller's existing systemd user manager and
cgroup v2. Portagenty does not gain root privileges, open a network listener, or
leave a resident Portagenty process behind. The systemd user manager already has
the same user-level authority needed to launch and stop the workload.

Ownership is fail-closed. A machine-local v2 receipt under
`$XDG_STATE_HOME/portagenty/` records the logical workspace/session identity,
opaque transient-unit name, systemd `InvocationID`, exact `ControlGroup`, exact
private multiplexer target, resolved limits, requested slice, and workload-anchor
evidence (nonce, marker, PID, and `/proc` start time). Current v2 receipts and
pending journals may also carry optional Linux boot-ID provenance. These UUID
strings are non-authoritative hints: malformed or missing values do not invalidate
the store and only disable direct prior-boot Enter. A pending-launch journal
protects the interval between transient-unit creation and durable receipt
persistence. The store uses a private directory, mode-`0600` files, advisory
locking, same-directory temporary writes, fsync, and atomic rename.

Before receipt finalization, every snapshot, and every systemd control action,
Portagenty reads back service placement and limits, resolves the invocation again,
and requires exact unit-name, invocation-ID, control-group, canonical cgroup path,
user-manager subtree, exact private tmux socket/session/pane PID or Zellij session/runtime target, workload PID/start-time/nonce, and exact
root cgroup agreement. Descendants are followed only through bounded
`/proc/<pid>/task/<tid>/children` traversal for every thread ID; there is no global `/proc` scan.
Traversal and symlink escapes from `/sys/fs/cgroup` are rejected. An escaped root
cannot become owned. Escaped descendants produce split containment, withholding
whole-workload metrics and control; a `build-contained` descendant in
`background.slice` is identified as an external bounded scope rather than claimed.

Existing v1 services remain legacy/restart-required exact-target attach-only until
they exit and are launched normally under v2. Portagenty does not auto-stop,
upgrade, or migrate them. Both the stored unit and target absent may permit
signal-free stale cleanup; partial presence is ambiguous and disables control.
Only a successful, error-free current-v2 stale reconciliation plus valid unequal
launch/current boot IDs authorizes routine Enter to bypass the replacement modal.
The existing locked coordinator still revalidates the exact receipt, unit, target,
marker, pending absence, and races before relaunch and sends no signal to the old
workload. An actual stale row with same, missing, invalid, or unreadable boot
evidence retains replacement confirmation. Pending, ambiguous, errored, and
unreconciled evidence blocks Enter; split containment attaches only to its exact
private target. `S` remains custom-limit editing. `x` is row-scoped: stale means
signal-free cleanup-only, owned means confirmed graceful/non-force stop, and
unmanaged live means confirmed multiplexer-native kill. `X` remains separately
confirmed force-kill. No boot hint causes startup or bulk launch.
Pending launches record exact creator process proof and block attach, fallback,
creation, stop, and kill. A valid unequal stored/current boot ID proves only that
the creator is gone; same, missing, invalid, or unreadable boot evidence retains
the PID/start-time check. They may be signal-free cleaned only when the creator,
exact unit, exact private target, and exact owner-runtime marker are all absent;
artifact probe errors and partial presence remain ambiguous. Existing shared
tmux/Zellij sessions are never retroactively claimed.

Supervised tmux uses a private mode-`0700` runtime/socket directory and one server
per logical session. Supervised Zellij uses an exact validated runtime directory,
a mode-`0600` generated layout, and PTY file descriptors passed through D-Bus.
Both launch the same owner-only one-shot workload-anchor protocol. Launch specs
and markers are constrained to `$XDG_RUNTIME_DIR/portagenty/workloads` with exact
nonce filenames; marker protocol, nonce, PID, and start time are verified before
unlink. Cleanup validates the exact runtime/path/nonce shape first. An absent
`portagenty` directory, `workloads` directory, or marker is already clean and is
not recreated; any existing component still must pass non-symlink, owner, mode,
type, protocol, and content checks before the exact marker is unlinked. The
private tmux server receives neither `XDG_RUNTIME_DIR` nor
`DBUS_SESSION_BUS_ADDRESS`, preventing tmux-created sibling scopes, while the pane
receives exact restored user-bus values.

The transient service receives typed argv and an explicitly constructed
environment. Multiplexer/systemd activation variables such as `TMUX*`,
`ZELLIJ*`, `INVOCATION_ID`, `NOTIFY_SOCKET`, `LISTEN_*`, and `WATCHDOG_*` are
stripped before launch; a validated runtime directory and declared session env
remain. Workspace commands are still user-authored code and execute with the
user's permissions.

Only explicit `kind = "claude-code"` selects Claude containment; names and
commands do not. Claude-kind services request `claude-code.slice`,
`ManagedOOMPreference=omit`, `MemoryHigh`, `MemoryMax`, `MemorySwapMax`, CPU quota,
and a finite per-service `TasksMax`. Portagenty first verifies that the externally managed aggregate
slice exists beneath `/claude.slice/claude-code.slice` with finite positive
`MemoryHigh`, `MemoryMax`, `MemorySwapMax`, and CPU quota, consistent memory
controls, and oomd-omit metadata. Aggregate `TasksMax` is optional and may remain
infinity; Portagenty never creates or modifies that slice. Claude overrides may only tighten the standard `3G`/`5G`/`512MiB`/
`800%`/`1200` policy. Generic sessions remain outside the Claude slice and receive
no inferred defaults.

`MemoryHigh` is a reclaim threshold; `MemoryMax` and `MemorySwapMax` are hard
ceilings; CPU quota throttles aggregate CPU; `TasksMax` rejects new tasks. These
limits reduce damage but are not a data-safety guarantee. Ordinary stop performs
exact multiplexer shutdown followed by revalidated non-force `StopUnit`;
`SendSIGKILL=no` prevents implicit escalation. Whole-cgroup SIGKILL is a separate
explicitly confirmed action available only for complete owned-and-verified v2
containment. Bulk stop never force-escalates and skips legacy, split, pending,
stale, and ambiguous targets.

Resource observation reads numeric cgroup files only. Portagenty does not inspect
terminal contents, prompts, command output, or agent logs, and does not retain
resource history or transmit telemetry. Same-user arbitrary code can edit the
receipt store or directly call systemd; that is within the existing out-of-scope
assumption of arbitrary local code execution, but receipt tampering must still
produce revalidation failure rather than control of a different workload.

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
