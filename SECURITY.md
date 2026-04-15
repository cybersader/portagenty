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
  etc.). Please report those upstream; we'll track them via
  `cargo audit` and update when fixed.
- Threats that assume arbitrary code execution on the user's machine
  (portagenty inherits the user's permissions).
- Issues specific to an agent CLI launched via a session (those are
  the agent's concern, not portagenty's).

## Current known issues

Tracked via `cargo audit`; none are blocking as of the latest release.
See the GitHub Actions `ci` workflow.
