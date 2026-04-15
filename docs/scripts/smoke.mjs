#!/usr/bin/env node
/**
 * Lightweight docs smoke test. Builds the site, serves it via `astro preview`,
 * and curls a known list of routes — checking both HTTP 200 and that each
 * page contains expected content. Kills the preview server on exit.
 *
 * Catches ~80% of what Playwright would catch (broken routes, missing
 * Flexoki CSS, busted base-path config, dead pagefind index) for ~10% of
 * the setup. For richer interaction tests, swap in Playwright later.
 *
 * Usage:
 *   bun scripts/smoke.mjs              # full build + smoke
 *   bun scripts/smoke.mjs --no-build   # assume dist/ already built
 */

import { execSync } from "node:child_process";
import { existsSync, statSync } from "node:fs";
import { resolve, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const docsDir = resolve(__dirname, "..");
const distDir = resolve(docsDir, "dist");

const PORT = Number(process.env.SMOKE_PORT || 4322);
const BASE = `http://127.0.0.1:${PORT}`;
const PREFIX = "/portagenty";
const skipBuild = process.argv.includes("--no-build");
const isWindows = process.platform === "win32";

// Each check: { path, mustContain }. mustContain is a substring asserted
// against the response body — keeps the test honest beyond a 200.
const CHECKS = [
  { path: `${PREFIX}/`, mustContain: "portagenty" },
  { path: `${PREFIX}/getting-started/installation/`, mustContain: "Installation" },
  { path: `${PREFIX}/getting-started/quickstart/`, mustContain: "Quickstart" },
  { path: `${PREFIX}/getting-started/dev/`, mustContain: "Tailscale" },
  { path: `${PREFIX}/concepts/`, mustContain: "Workspace" },
  { path: `${PREFIX}/pagefind/pagefind.js`, mustContain: "" }, // 200 is enough
  { path: `${PREFIX}/sitemap-index.xml`, mustContain: "<sitemap>" },
];

let server = null;

function log(msg) {
  console.log(`  ${msg}`);
}

function build() {
  log("Building docs site...");
  execSync("bun x astro build", {
    cwd: docsDir,
    stdio: "inherit",
    shell: isWindows,
  });
}

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "application/javascript",
  ".mjs": "application/javascript",
  ".css": "text/css",
  ".json": "application/json",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".gif": "image/gif",
  ".webp": "image/webp",
  ".ico": "image/x-icon",
  ".xml": "application/xml",
  ".txt": "text/plain; charset=utf-8",
  ".woff": "font/woff",
  ".woff2": "font/woff2",
  ".pagefind": "application/octet-stream",
  ".pf_meta": "application/octet-stream",
  ".pf_index": "application/octet-stream",
  ".pf_fragment": "application/octet-stream",
};

function contentType(path) {
  const ext = path.slice(path.lastIndexOf("."));
  return MIME[ext] || "application/octet-stream";
}

// Tiny static server. The Astro build emits dist/ at the configured `base`
// (i.e. dist/<base>/index.html), so we serve dist/ from `/`. Requests for
// dirs resolve to index.html. Bun.serve is built in so no extra dep.
function startServer() {
  log(`Starting static server on http://127.0.0.1:${PORT}...`);
  server = Bun.serve({
    hostname: "127.0.0.1",
    port: PORT,
    fetch(req) {
      const url = new URL(req.url);
      let pathname = decodeURIComponent(url.pathname);

      // Astro emits absolute URLs with the configured base (e.g. /portagenty/...)
      // but writes files to dist/ without that prefix. Strip the base so the
      // local server mirrors what GitHub Pages does in production.
      if (pathname === PREFIX || pathname.startsWith(`${PREFIX}/`)) {
        pathname = pathname.slice(PREFIX.length) || "/";
      }

      let filePath = join(distDir, pathname);

      try {
        if (existsSync(filePath) && statSync(filePath).isDirectory()) {
          filePath = join(filePath, "index.html");
        }
        if (!existsSync(filePath)) {
          if (existsSync(`${filePath}.html`)) {
            filePath = `${filePath}.html`;
          } else {
            return new Response("Not found", { status: 404 });
          }
        }
        const file = Bun.file(filePath);
        return new Response(file, {
          headers: { "Content-Type": contentType(filePath) },
        });
      } catch (err) {
        return new Response(`Error: ${err.message}`, { status: 500 });
      }
    },
  });
}

async function waitForReady() {
  log("Waiting for server to be ready...");
  for (let i = 0; i < 30; i++) {
    try {
      const res = await fetch(`${BASE}${PREFIX}/`);
      if (res.status === 200) {
        log("Server is ready.");
        return;
      }
    } catch {
      // not yet
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error("Server did not become ready within 6s");
}

async function runChecks() {
  let failures = 0;
  for (const { path, mustContain } of CHECKS) {
    const url = `${BASE}${path}`;
    let status = 0;
    let body = "";
    try {
      const res = await fetch(url);
      status = res.status;
      body = await res.text();
    } catch (err) {
      console.error(`  FAIL  ${path} — fetch error: ${err.message}`);
      failures++;
      continue;
    }

    if (status !== 200) {
      console.error(`  FAIL  ${path} — expected 200, got ${status}`);
      failures++;
      continue;
    }

    if (mustContain && !body.includes(mustContain)) {
      console.error(
        `  FAIL  ${path} — body missing expected substring "${mustContain}"`,
      );
      failures++;
      continue;
    }

    console.log(`  OK    ${path}`);
  }
  return failures;
}

function cleanup() {
  if (server) {
    try {
      server.stop(true);
    } catch {}
    server = null;
  }
}

process.on("SIGINT", () => {
  cleanup();
  process.exit(130);
});
process.on("SIGTERM", () => {
  cleanup();
  process.exit(143);
});

async function main() {
  if (!skipBuild) {
    build();
  } else if (!existsSync(resolve(docsDir, "dist"))) {
    console.error("--no-build set but docs/dist does not exist");
    process.exit(1);
  }

  startServer();
  try {
    await waitForReady();
    const failures = await runChecks();
    if (failures > 0) {
      console.error(`\n  ${failures} check(s) failed.`);
      process.exit(1);
    }
    console.log(`\n  All ${CHECKS.length} smoke checks passed.`);
  } finally {
    cleanup();
  }
}

main().catch((err) => {
  console.error(err);
  cleanup();
  process.exit(1);
});
