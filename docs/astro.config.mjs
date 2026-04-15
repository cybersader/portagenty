// @ts-check
import { defineConfig } from "astro/config";
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
          autogenerate: { directory: "getting-started" },
        },
        {
          label: "Concepts",
          autogenerate: { directory: "concepts" },
        },
        {
          label: "Reference",
          autogenerate: { directory: "reference" },
        },
        {
          label: "Design",
          autogenerate: { directory: "design" },
          collapsed: true,
        },
      ],
    }),
  ],
});
