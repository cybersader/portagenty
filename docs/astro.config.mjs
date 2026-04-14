// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import starlightImageZoom from "starlight-image-zoom";
import starlightThemeFlexoki from "starlight-theme-flexoki";

// https://astro.build/config
export default defineConfig({
  site: "https://cybersader.github.io",
  base: "/portagenty",
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
          label: "Design",
          autogenerate: { directory: "design" },
          collapsed: true,
        },
        {
          label: "Reference",
          autogenerate: { directory: "reference" },
          collapsed: true,
        },
      ],
    }),
  ],
});
