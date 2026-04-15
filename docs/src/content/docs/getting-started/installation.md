---
title: Installation
description: How to install the portagenty binary (`pa`).
sidebar:
  order: 1
---

## Prerequisites

- A Rust toolchain (1.82 or newer) if building from source.
- **tmux** must be installed and on `PATH`. Zellij and WezTerm
  adapters land in v1.x; v1 is tmux-only.

## Build from source

```sh
git clone https://github.com/cybersader/portagenty.git
cd portagenty
cargo install --path .
# or: cargo build --release && cp target/release/pa ~/.local/bin/
```

## Verify

```sh
pa --version
pa --help
```

## Prebuilt binaries

Planned for v1.x. Until then, `cargo install --path .` gives you a
locally-compiled `pa` that matches your platform exactly. See the
[roadmap](https://github.com/cybersader/portagenty/blob/main/ROADMAP.md)
for tracking.

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
