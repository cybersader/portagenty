---
title: Termux + SSH
description: Running pa on Android via Termux + SSH to a desktop.
sidebar:
  order: 4
---

portagenty never runs *on* Termux. The typical mobile path is:

```
Termux on Android ──SSH──▶ your desktop ──▶ pa ──▶ zellij / tmux
```

Your sessions persist on the desktop; Termux is just the transport. Run
bare `pa` from the SSH shell, choose a workspace with a live badge, and
PortAgenty attaches to the existing multiplexer session. On Linux it also
recovers the secure systemd runtime directory for Zellij when the SSH login
omits `XDG_RUNTIME_DIR`, so no export, alias, or wrapper is required.

## Supported interactions

The TUI is designed around Termux's on-screen keyboard constraints
(DESIGN §10). Specifically:

| What you want | How to do it in Termux |
|-----|-----|
| Move selection | `j` / `k` (letter keys) or `↓` / `↑` from the Extra Keys row |
| Jump to top | `g`, `Home` |
| Jump to bottom | `G`, `End` |
| Launch | `Enter` |
| Quit | `q`, `Esc`, or `Ctrl-C` (Volume-Down-as-Ctrl works fine) |

No shortcut requires `Alt`, `Meta`, or `Fn`. Everything that's a
letter key also has a non-letter fallback (`↓` for `j`, `Home` for
`g`, etc.) for whichever keyboard you're using.

## Footer hints adapt to width

At narrow widths (typical phone portrait, ~30-45 cols), portagenty
drops the full hint line for a shorter one so `q: quit` is always
visible:

| Terminal width | Footer |
|---|---|
| `≥ 60` cols | `j/k: nav · g/G: top/bottom · Enter: launch · q: quit` |
| `≥ 30` cols | `j/k · Enter: launch · q: quit` |
| `< 30` cols | `q: quit` |

## Recommended setup

1. On desktop: install `pa`, install `tmux` or Zellij, and run an SSH server.
2. On phone: install Termux + Termux:Widget; set up SSH keys.
3. SSH to the desktop and run bare `pa`. The global picker shows registered
   workspaces and their live-session counts; select one to attach or create it.
4. Detach from the multiplexer when leaving. On the next desktop or mobile
   connection, run `pa` again and select the same live workspace session.

Do not run `pa` from inside an existing Zellij client: Zellij refuses nested
attachments. Detach to the SSH shell first, then use `pa` as the launcher.
For the broader terminal setup, see
[`cybersader/agentic-workflow-and-tech-stack`'s terminal-setup docs](https://github.com/cybersader/agentic-workflow-and-tech-stack/blob/main/docs/terminal-setup.md).

## Bootstrapping a new project from the phone

You don't need to SSH to desktop, open nano, and hand-edit a TOML
file just to get started on a new project. From Termux:

```sh
ssh desktop
cd ~/code/new-project
pa init                                            # scaffolds <dirname>.portagenty.toml
pa add claude -c "claude --resume" --kind claude-code
pa add dev   -c "bun run dev"      --kind dev-server
pa                                                  # TUI, pick one, Enter
```

Four commands and you're in. No editor required. Each `pa add`
appends to the workspace file non-destructively; existing
comments and ordering are preserved.

When you come back to the project from the desktop, everything's
already in git-friendly TOML ready to commit.

## Known hiccups

- **Flow control (`Ctrl+S` / `Ctrl+Q`)** can freeze the terminal if
  your shell doesn't disable it. Most Termux setups handle this;
  if you see the TUI appear to hang, press `Ctrl+Q` (XON) to
  un-freeze.
- **Very short terminals (< 3 rows)** don't leave room for the
  header+body+footer split. Widen the window or rotate to
  landscape.
