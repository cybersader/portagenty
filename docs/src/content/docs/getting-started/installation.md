---
title: Installation
description: How to install the portagenty binary (`pa`).
sidebar:
  order: 1
---

## One command (with Rust)

```sh
cargo install --git https://github.com/cybersader/portagenty
```

This downloads the repo, builds a release binary, and drops it at
`~/.cargo/bin/pa`. No manual clone, no `cd`, no `git pull` to stay
current — just re-run the same command whenever you want to update.

## Don't have Rust yet?

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
cargo install --git https://github.com/cybersader/portagenty
```

Three commands, works on Linux / macOS / WSL. On Windows native,
install Rust via <https://rustup.rs/> then run the `cargo install`
line in PowerShell or CMD.

## Install a multiplexer

`pa` doesn't bundle its own — it drives tmux or zellij. You need at
least one:

- **tmux** (recommended for cross-device takeover — `pa` uses
  `tmux attach -d` internally):
  ```sh
  sudo apt install tmux        # Debian / Ubuntu / WSL
  brew install tmux            # macOS
  ```
- **zellij** (alternative, better integration with OpenCode):
  see <https://zellij.dev/documentation/installation>.

WezTerm is a deferred non-fit for portagenty's cross-device model —
see [the roadmap](https://github.com/cybersader/portagenty/blob/main/ROADMAP.md#vx--followons)
for the rationale.

## Verify

```sh
pa --version
pa --help
```

If `pa: command not found`, make sure `~/.cargo/bin` is on your
`PATH`. Rustup's installer adds it by default.

## Prebuilt binaries

Planned for v1.x. Until then, the `cargo install --git` one-liner is
the recommended path — it builds locally against your exact platform
and toolchain.

## Smoke-test your setup

```sh
cat > /tmp/smoke.portagenty.toml <<'EOF'
name = "Smoke"
multiplexer = "tmux"

[[session]]
name = "shell"
cwd = "/tmp"
command = "bash"
EOF

pa list -w /tmp/smoke.portagenty.toml
pa launch shell -w /tmp/smoke.portagenty.toml
```

The first command should print the resolved workspace; the second
attaches you to a new tmux session running `bash` in `/tmp`. Detach
with `Ctrl-b d`.

## Smoke-test your setup

```sh
cat > /tmp/smoke.portagenty.toml <<'EOF'
name = "Smoke"
multiplexer = "tmux"

[[session]]
name = "shell"
cwd = "/tmp"
command = "bash"
EOF

pa list -w /tmp/smoke.portagenty.toml
pa launch shell -w /tmp/smoke.portagenty.toml
```

The first command should print the resolved workspace; the second
attaches you to a new tmux session running `bash` in `/tmp`. Detach
with `Ctrl-b d`.
