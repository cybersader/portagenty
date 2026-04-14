---
title: Installation
description: How to install the portagenty binary (`pa`).
sidebar:
  order: 1
---

:::caution
portagenty is **pre-release**. The binary `pa` is under construction. The
sections below describe the intended install paths; not all are wired up
yet. Track progress in the
[roadmap](https://github.com/cybersader/portagenty/blob/main/ROADMAP.md).
:::

## Build from source

```sh
git clone https://github.com/cybersader/portagenty.git
cd portagenty
cargo build --release
# Binary at target/release/pa
```

## Prebuilt binaries (planned)

Single static binaries for Linux (musl), macOS (universal), and Windows
will be published from CI on tagged releases. Until then, build from
source.

## Verify

```sh
pa --version
pa --help
```
