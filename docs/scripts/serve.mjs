#!/usr/bin/env node
/**
 * portagenty docs serve — interactive menu for local docs preview + sharing.
 *
 *   bun run serve              — interactive menu
 *   bun run serve dev          — Astro dev server with HMR (LAN-bindable)
 *   bun run serve preview      — build docs + serve dist (production preview)
 *   bun run serve build        — docs build only → docs/dist
 *   bun run serve tailscale    — Astro dev shared via Tailscale (tailnet only)
 *   bun run serve cloudflare   — Astro dev shared via Cloudflare Tunnel (public)
 *
 * Pattern adapted from cybersader/crosswalker's scripts/serve.mjs. Covers the
 * cross-OS rollup-binary contamination problem (WSL ↔ Windows) and the local
 * Tailscale share workflow.
 */

import { spawn, execSync } from "node:child_process";
import { existsSync, rmSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createInterface } from "node:readline";

const __dirname = dirname(fileURLToPath(import.meta.url));
const docsDir = resolve(__dirname, "..");

const DOCS_PORT = 4321;
const isWindows = process.platform === "win32";

const mode = process.argv[2] || "interactive";
const children = [];

function log(msg) {
  console.log(`  ${msg}`);
}
function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

function run(cmd) {
  try {
    return execSync(cmd, { stdio: "pipe", timeout: 30000, shell: true })
      .toString()
      .trim();
  } catch {
    return "";
  }
}

function hasCmd(name) {
  return isWindows
    ? !!run(`where ${name} 2>nul`)
    : !!run(`which ${name} 2>/dev/null`);
}

// Rollup ships its native binary as a per-platform optional dep. If
// docs/node_modules was installed on a different OS (e.g. WSL → Windows), the
// wrong binary is present and astro dev crashes with
// "Cannot find module @rollup/rollup-<platform>". Detect and force a clean
// reinstall when that happens.
function rollupNativePkg() {
  const { platform, arch } = process;
  if (platform === "win32" && arch === "x64")
    return "@rollup/rollup-win32-x64-msvc";
  if (platform === "win32" && arch === "arm64")
    return "@rollup/rollup-win32-arm64-msvc";
  if (platform === "linux" && arch === "x64")
    return "@rollup/rollup-linux-x64-gnu";
  if (platform === "linux" && arch === "arm64")
    return "@rollup/rollup-linux-arm64-gnu";
  if (platform === "darwin" && arch === "x64") return "@rollup/rollup-darwin-x64";
  if (platform === "darwin" && arch === "arm64")
    return "@rollup/rollup-darwin-arm64";
  return null;
}

function ensureDocsDeps() {
  const nodeModules = resolve(docsDir, "node_modules");
  let needsInstall = false;
  let needsNuke = false;

  if (!existsSync(nodeModules)) {
    needsInstall = true;
  } else {
    const expectedRollup = rollupNativePkg();
    if (expectedRollup && !existsSync(resolve(nodeModules, expectedRollup))) {
      log(
        `docs/node_modules is missing ${expectedRollup} — likely installed on a different OS.`,
      );
      needsNuke = true;
      needsInstall = true;
    }
  }

  if (needsNuke) {
    log("Removing stale docs/node_modules...");
    rmSync(nodeModules, { recursive: true, force: true });
    const lock = resolve(docsDir, "bun.lock");
    if (existsSync(lock)) {
      log("Removing docs/bun.lock...");
      rmSync(lock, { force: true });
    }
  }

  if (needsInstall) {
    log("Installing docs/ dependencies (bun install)...");
    execSync("bun install", { cwd: docsDir, stdio: "inherit", shell: true });
    log("docs dependencies installed.");
  }
}

function getTailscale() {
  if (hasCmd("tailscale")) return "tailscale";
  if (hasCmd("tailscale.exe")) return "tailscale.exe";
  return null;
}

function hasCloudflared() {
  return hasCmd("cloudflared") || !!run("bun x cloudflared --version 2>/dev/null");
}

function track(child) {
  children.push(child);
  return child;
}

function cleanup() {
  for (const c of children) {
    try {
      c.kill();
    } catch {}
  }
  const ts = getTailscale();
  if (ts) run(`${ts} serve off 2>/dev/null`);
}

process.on("SIGINT", () => {
  cleanup();
  process.exit(0);
});
process.on("SIGTERM", () => {
  cleanup();
  process.exit(0);
});

async function prompt() {
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  const ts = getTailscale();
  const cf = hasCloudflared();

  console.log("\n  ━━━ portagenty docs serve ━━━\n");
  console.log(`  1) Dev server (HMR)         http://localhost:${DOCS_PORT}`);
  console.log(`  2) Preview built site       http://localhost:${DOCS_PORT}`);
  console.log(`  3) Build only               → docs/dist`);
  console.log(
    `  4) Share via Tailscale      ${ts ? "tailnet only" : "(not installed)"}`,
  );
  console.log(
    `  5) Share via Cloudflare     ${cf ? "public URL" : "(cloudflared not found)"}\n`,
  );

  return new Promise((res) => {
    rl.question("  Choose [1-5]: ", (a) => {
      rl.close();
      const map = {
        1: "dev",
        2: "preview",
        3: "build",
        4: "tailscale",
        5: "cloudflare",
      };
      res(map[a.trim()] || "dev");
    });
  });
}

function startDev() {
  ensureDocsDeps();
  log("Starting Astro dev server (HMR, --host 0.0.0.0)...");
  const child = track(
    spawn(
      "bun",
      [
        "x",
        "astro",
        "dev",
        "--host",
        "0.0.0.0",
        "--port",
        String(DOCS_PORT),
      ],
      { cwd: docsDir, stdio: "inherit", shell: isWindows },
    ),
  );
  child.on("exit", (code) => {
    if (code !== 0 && code !== null) {
      console.error(`\n  Dev server exited with code ${code}`);
      cleanup();
      process.exit(code);
    }
  });
  return child;
}

function startPreview() {
  ensureDocsDeps();
  log("Building docs site...");
  const build = spawn("bun", ["x", "astro", "build"], {
    cwd: docsDir,
    stdio: "inherit",
    shell: isWindows,
  });
  return new Promise((res, rej) => {
    build.on("exit", (code) => {
      if (code !== 0) return rej(new Error(`build failed (exit ${code})`));
      log("Starting preview server (--host 0.0.0.0)...");
      track(
        spawn(
          "bun",
          [
            "x",
            "astro",
            "preview",
            "--host",
            "0.0.0.0",
            "--port",
            String(DOCS_PORT),
          ],
          { cwd: docsDir, stdio: "inherit", shell: isWindows },
        ),
      );
      res();
    });
  });
}

function runBuild() {
  ensureDocsDeps();
  log("Building docs site → docs/dist...");
  return new Promise((res, rej) => {
    const p = spawn("bun", ["x", "astro", "build"], {
      cwd: docsDir,
      stdio: "inherit",
      shell: isWindows,
    });
    p.on("exit", (code) => {
      if (code !== 0) return rej(new Error(`build failed (exit ${code})`));
      log("Build complete.");
      res();
    });
  });
}

async function tailscaleServe(port) {
  const ts = getTailscale();
  if (!ts) {
    log("Tailscale not found. Install it from https://tailscale.com.");
    return;
  }
  run(`${ts} serve off 2>/dev/null`);
  spawn(ts, ["serve", String(port)], { stdio: "ignore" });
  await sleep(2000);
  const status = run(`${ts} serve status 2>/dev/null`);
  const url = status.match(/(https:\/\/[^\s]+)/)?.[1];
  const ip = run(`${ts} ip -4 2>/dev/null`);
  console.log("");
  if (url) log(`Public-on-tailnet: ${url}`);
  if (ip) log(`Direct: http://${ip}:${port} (tailnet only)`);
  if (!url && !ip) log(`http://localhost:${port}`);
}

function startCloudflareTunnel(port) {
  log("Starting Cloudflare Tunnel...");
  return track(
    spawn(
      "bun",
      ["x", "cloudflared", "tunnel", "--url", `http://localhost:${port}`],
      { stdio: "inherit", shell: isWindows },
    ),
  );
}

async function main() {
  const chosen = mode === "interactive" ? await prompt() : mode;

  switch (chosen) {
    case "dev":
      startDev();
      break;
    case "preview":
      await startPreview();
      break;
    case "build":
      await runBuild();
      process.exit(0);
    case "tailscale":
      startDev();
      await sleep(3000);
      await tailscaleServe(DOCS_PORT);
      break;
    case "cloudflare":
      startDev();
      await sleep(3000);
      startCloudflareTunnel(DOCS_PORT);
      break;
    default:
      console.error(`Unknown mode: ${chosen}`);
      console.error(
        "Use one of: dev, preview, build, tailscale, cloudflare (or no arg for menu)",
      );
      process.exit(1);
  }
}

main().catch((err) => {
  console.error(err);
  cleanup();
  process.exit(1);
});
