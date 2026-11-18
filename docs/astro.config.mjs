// @ts-check
import { defineConfig } from "astro/config";
import { unified } from "@astrojs/markdown-remark";
import starlight from "@astrojs/starlight";
import starlightImageZoom from "starlight-image-zoom";
import starlightThemeFlexoki from "starlight-theme-flexoki";

// https://astro.build/config
export default defineConfig({
  // The repo is private; docs are served locally / over Tailscale via
  // `bun scripts/serve.mjs`. `site` and `base` are kept as the eventual
  // public URL for when the project goes public; harmless if unused.
  site: "https://cybersader.github.io",
  base: "/portagenty",
  // Image zoom needs the unified pipeline, not Astro 7's default Sätteri.
  markdown: { processor: unified() },
  vite: {
    server: {
      // Allow access from Docker / Tailscale / LAN / cross-machine previews.
      // Vite 6+ blocks non-localhost Host headers by default — this opens it
      // back up. Safe for local dev only; production builds are static files.
      allowedHosts: true,
    },
  },
  integrations: [
    starlight({
      title: "portagenty",
      description:
        "Portable, terminal-native launcher for agent workspaces.",
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/cybersader/portagenty",
        },
      ],
      editLink: {
        baseUrl:
          "https://github.com/cybersader/portagenty/edit/main/docs/",
      },
      lastUpdated: true,
      plugins: [
        starlightThemeFlexoki(),
        starlightImageZoom(),
      ],
      sidebar: [
        {
          label: "Getting started",
          items: [{ autogenerate: { directory: "getting-started" } }],
        },
        {
          label: "Features",
          slug: "features",
        },
        {
          label: "Concepts",
          items: [{ autogenerate: { directory: "concepts" } }],
        },
        {
          label: "Reference",
          items: [{ autogenerate: { directory: "reference" } }],
        },
      ],
    }),
  ],
});
