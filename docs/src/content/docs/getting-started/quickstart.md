---
title: Quickstart
description: Open the TUI and launch your first session.
sidebar:
  order: 2
---

:::caution
This page describes the v1 user flow. Most steps are not yet implemented.
For now, this is a preview of how things will work.
:::

## 1. Define a workspace

Create a `*.portagenty.toml` anywhere — typically at the root of a
directory that holds related projects.

```toml
name = "Example workspace"
multiplexer = "tmux"

[[session]]
project = "~/code/portagenty"
name = "claude"
cwd = "."
command = "claude"
```

## 2. Open the TUI

```sh
cd /path/with/workspace/file
pa
```

`pa` walks up from `$PWD` looking for a `*.portagenty.toml`. It loads the
workspace and shows its sessions. Nothing is launched yet.

## 3. Open a session

Use arrow keys (or `j`/`k`) to navigate; press `Enter` to attach. The
session is created on first open (workspace-scoped lazy launch — see
[Concepts](/portagenty/concepts/)).

## 4. Detach and reattach

Detach with the multiplexer's normal binding (`Ctrl-b d` for tmux). Run
`pa` again later and pick the same session — it reattaches to the live
process.
