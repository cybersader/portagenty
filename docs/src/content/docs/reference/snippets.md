---
title: Bundled snippets
description: Opt-in bash snippets that ship with pa for agentic-workflow ergonomics.
sidebar:
  order: 3
---

`pa` ships bundled, opt-in bash snippets. They're compiled into the
binary, you install them with one command, and subsequent installs
update the block in-place instead of duplicating. Nothing touches
your rc file until you explicitly run `install`.

## List what's available

```sh
pa snippets list
```

Current catalog:

| Name | What |
|---|---|
| `pa-aliases` | Short aliases for pa commands: `p`, `pl`, `pc`, `pls`, `pe`, `pi`, `pad` |
| `termux-friendly` | Mobile-SSH ergonomics: disables flow control, binds Ctrl+L/Ctrl+R reliably, adds `cc`/`ccc` aliases for Claude Code |

## Inspect before you install

```sh
pa snippets show pa-aliases
pa snippets show termux-friendly
```

Prints the snippet's contents to stdout. Read it, decide if it's
what you want, then install.

## Install

```sh
pa snippets install pa-aliases                   # appends to ~/.bashrc
pa snippets install termux-friendly --to ~/.zshrc
pa snippets install pa-aliases --dry-run         # preview the file after install
```

What happens under the hood:

- The snippet gets wrapped in begin/end markers tagged with its
  name:

  ```
  # >>> pa snippet: pa-aliases >>>
  # Installed by `pa snippets install pa-aliases`. Do not edit — re-run to update.
  alias p='pa'
  alias pl='pa launch'
  ...
  # <<< pa snippet: pa-aliases <<<
  ```

- Subsequent `install` calls **replace** the block between the
  markers instead of appending. Idempotent: run it ten times, end
  up with one copy.

- Other content in the rc file (your PS1, your own aliases, etc.)
  is preserved verbatim.

Then reload your shell to pick up the changes:

```sh
source ~/.bashrc
```

## Uninstall

```sh
pa snippets uninstall pa-aliases
pa snippets uninstall termux-friendly --from ~/.zshrc
pa snippets uninstall pa-aliases --dry-run
```

Removes everything between the markers. If the snippet isn't
installed, says so and exits cleanly.

## Not-bash shells

The snippets are POSIX-ish and work as-is under zsh (`--to ~/.zshrc`).
Fish uses a different syntax entirely — fish snippets aren't
bundled yet. See [the roadmap](https://github.com/cybersader/portagenty/blob/main/ROADMAP.md)
if you want to track that, or contribute one.

## Why bundled, not external

The bar for an opinionated snippet shipping with `pa` is: "most
people running `pa` would benefit from this and it aligns with the
agentic-workflow story `pa` exists to serve." That keeps the
catalog small and curated — scripts you'd otherwise paste from
random docs sites, now as code-reviewed + tested fragments living
inside the binary. Your own per-machine tweaks stay in your rc file
outside the pa-managed blocks.
