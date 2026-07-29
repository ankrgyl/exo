import { defineConfig } from "vitepress";

// Docs are served under exoharness.ai/docs by the Cloudflare Worker in
// website/. `vitepress build` emits static files straight into website/dist/docs
// (outDir below), which the Worker serves as plain assets — no nested install,
// no separate deploy.
export default defineConfig({
  base: "/docs/",
  outDir: "../dist/docs",
  cleanUrls: true,
  lang: "en",
  title: "exo docs",
  description: "Documentation for exo — a minimal system for building agents.",
  appearance: "dark",
  head: [
    ["link", { rel: "icon", type: "image/svg+xml", href: "/favicon.svg" }],
  ],
  themeConfig: {
    logo: {
      light: "/images/exo-badge-light.svg",
      dark: "/images/exo-badge-dark.svg",
    },
    siteTitle: "exo",
    nav: [
      { text: "Home", link: "/" },
      { text: "GitHub", link: "https://github.com/exoharness/exo" },
    ],
    search: { provider: "local" },
    socialLinks: [{ icon: "github", link: "https://github.com/exoharness/exo" }],
    sidebar: [
      { text: "Overview", link: "/" },
      {
        text: "Getting Started",
        link: "/getting-started/",
        collapsed: false,
        items: [
          { text: "Installation", link: "/getting-started/installation" },
          { text: "Your First Session", link: "/getting-started/first-session" },
          { text: "Using the CLI Directly", link: "/getting-started/quick-start" },
          {
            text: "A Sandboxed Conversation",
            link: "/getting-started/sandboxed-conversation",
          },
        ],
      },
      {
        text: "Concepts",
        link: "/concepts/",
        collapsed: false,
        items: [
          {
            text: "Exoharness & Executor",
            link: "/concepts/exoharness-and-executor",
          },
          { text: "Data Model", link: "/concepts/data-model" },
          { text: "Lifecycles", link: "/concepts/lifecycles" },
          { text: "Time Travel", link: "/concepts/time-travel" },
          { text: "Sandboxes", link: "/concepts/sandboxes" },
          {
            text: "Bindings & Secrets",
            link: "/concepts/bindings-and-secrets",
          },
          { text: "Executors & Harnesses", link: "/concepts/executors" },
          { text: "Tools", link: "/concepts/tools" },
          { text: "Adapters", link: "/concepts/adapters" },
          { text: "Task Scheduler", link: "/concepts/task-scheduler" },
          { text: "The Canonical Agent", link: "/concepts/canonical-agent" },
        ],
      },
      {
        text: "Tutorials",
        link: "/tutorials/",
        collapsed: false,
        items: [
          {
            text: "Custom Agent Quickstart",
            link: "/tutorials/write-your-own-agent",
          },
          { text: "Custom Coding Agent", link: "/tutorials/custom-coding-agent" },
          {
            text: "Game & Third-Party Tool Integration",
            link: "/tutorials/game-emulator-integration",
          },
        ],
      },
      { text: "Development", link: "/development/" },
    ],
  },
});
