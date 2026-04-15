---
title: Shell completion
description: Turn on tab completion for pa subcommands and flags.
sidebar:
  order: 5
---

`pa completions <shell>` emits a shell completion script. Pipe it
into wherever your shell looks for completions and you get Tab
completion for every subcommand and every flag.

## bash

Put the script in your user completions directory. Most bash setups
read `~/.local/share/bash-completion/completions/*`:

```sh
mkdir -p ~/.local/share/bash-completion/completions
pa completions bash > ~/.local/share/bash-completion/completions/pa
```

Then open a new shell (or `source ~/.bashrc`). Try it:

```sh
pa <Tab><Tab>              # shows subcommands
pa launch --<Tab>          # shows flags for launch
```

## zsh

Drop the script into a directory on your `fpath`. A common spot:

```sh
mkdir -p ~/.zsh/completions
pa completions zsh > ~/.zsh/completions/_pa

# Add to ~/.zshrc if not already there:
fpath=(~/.zsh/completions $fpath)
autoload -Uz compinit && compinit
```

Reload your shell after that.

## fish

Fish auto-loads from `~/.config/fish/completions/`:

```sh
pa completions fish > ~/.config/fish/completions/pa.fish
```

Completion works immediately in new fish shells.

## What completes today

- **Subcommand names** (`pa la<Tab>` → `launch`)
- **Flag names** (`pa launch --<Tab>` → `--workspace`, `--dry-run`,
  `--shared`)
- **Flag values** that come from a closed set (e.g.
  `pa launch --help`, `pa init --mpx <Tab>` →
  `tmux zellij`)

## What doesn't yet

Dynamic completions that require loading the current workspace
aren't wired up in v1.x. Things that will *not* tab-complete yet:

- Session names for `pa launch`, `pa claim`, `pa rm`, `pa edit`
- Snippet names for `pa snippets show / install / uninstall`
- Workspace file paths for `-w` (falls back to generic filesystem
  completion provided by your shell, which works fine)

These are tracked in the
[roadmap](https://github.com/cybersader/portagenty/blob/main/ROADMAP.md)
under the v1.x follow-ons list. For now, `pa list` prints every
known session name so you can see what to type.
