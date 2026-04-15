---
title: Running the docs locally
description: Local docs preview, with optional Tailscale or Cloudflare share.
sidebar:
  order: 3
---

The docs site is served from your machine. The repo is private, so there's
no public deploy — but the `serve.mjs` orchestrator gives you LAN, Tailscale,
or Cloudflare access in one command.

## Quick start

```sh
cd docs
bun install        # first time only
bun run serve      # interactive menu
```

The menu offers:

| # | Mode | Notes |
|---|---|---|
| 1 | Dev (HMR) | `astro dev --host 0.0.0.0` — your LAN can hit it on port 4321 |
| 2 | Preview built site | Build then `astro preview --host 0.0.0.0` |
| 3 | Build only | Static output to `docs/dist` |
| 4 | Tailscale share | `tailscale serve 4321`, prints public-on-tailnet URL + direct tailnet IP |
| 5 | Cloudflare Tunnel | `cloudflared tunnel --url http://localhost:4321` for a true public URL |

## Non-interactive shortcuts

```sh
bun run serve:dev          # mode 1
bun run serve:preview      # mode 2
bun run serve:build        # mode 3
bun run serve:tailscale    # mode 4
bun run serve:cloudflare   # mode 5
```

## WSL ↔ Windows hop

The orchestrator auto-detects when `docs/node_modules` was installed on the
wrong OS (rollup ships a per-platform native binary; switching from WSL to
PowerShell without reinstalling makes Astro crash). When that happens it
nukes `docs/node_modules` and `docs/bun.lock` and reinstalls cleanly.

This pattern is lifted from
[`cybersader/crosswalker`'s `scripts/serve.mjs`](https://github.com/cybersader/crosswalker/blob/main/scripts/serve.mjs).
