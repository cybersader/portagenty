---
title: Termux + SSH
description: Running pa on Android via Termux + SSH to a desktop.
sidebar:
  order: 4
---

portagenty never runs *on* Termux. The typical mobile path is:

```
Termux on Android ──SSH──▶ your desktop ──▶ zellij / tmux ──▶ pa
```

Your sessions persist on the desktop; Termux is just the transport.

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

1. On desktop: install `pa`, install `tmux`, run SSH server.
2. On phone: install Termux + Termux:Widget; set up SSH keys.
3. Persist sessions across disconnects: run `pa` inside a zellij or
   tmux session on the desktop so you can re-attach after a flaky
   mobile connection. For zellij, see the broader pattern in
   [`cybersader/agentic-workflow-and-tech-stack`'s terminal-setup
   docs](https://github.com/cybersader/agentic-workflow-and-tech-stack/blob/main/docs/terminal-setup.md).

## Known hiccups

- **Flow control (`Ctrl+S` / `Ctrl+Q`)** can freeze the terminal if
  your shell doesn't disable it. Most Termux setups handle this;
  if you see the TUI appear to hang, press `Ctrl+Q` (XON) to
  un-freeze.
- **Very short terminals (< 3 rows)** don't leave room for the
  header+body+footer split. Widen the window or rotate to
  landscape.
