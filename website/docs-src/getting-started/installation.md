---
title: Installation
description: One setup script to a running agent.
---

# Installation

## Prerequisites

The setup script checks for these and prints per-platform install
instructions for anything missing:

- **git**
- **Node.js 22+ and pnpm**
- **Rust** (via [rustup](https://rustup.rs/))
- **Docker** — running, for the agent's sandbox

You'll also need an API key from **OpenAI**, **OpenRouter**, or **Venice**.

## Run the setup script

```bash
curl -fsSL https://raw.githubusercontent.com/exoharness/exo/main/setup.sh -o setup.sh
bash setup.sh
```

For a non-interactive provider selection, set the provider, model, and API key
in the environment:

```bash
EXO_MODEL_PROVIDER=venice \
EXO_UPSTREAM_MODEL=zai-org-glm-5 \
VENICE_API_KEY=vapi_... \
bash setup.sh
```

Venice uses its OpenAI-compatible Chat Completions endpoint at
`https://api.venice.ai/api/v1`.

The script installs Exo into the current directory and walks you through
everything:

1. Clones the repository and builds the `exo` CLI.
2. Asks you to choose OpenAI, OpenRouter, or Venice and enter its API key
   (stored in a `.env` file with `600` permissions, then registered in exo's
   secret store).
3. Asks for your name and your agent's name, and writes a local profile at
   `.exo/exo-profile.md` (git-ignored — machine-specific instructions
   live here).
4. Starts the canonical agent: a sandbox (Ubuntu 24.04 in Docker), the task
   scheduler, and the ExoChat adapter.

When it finishes, two things happen:

- It prints a URL like
  `https://exoharness.ai/chat?role=user&c=...#k=...` — a minimal remote chat
  interface to your agent. Open it in any browser, including your phone.
- It drops you into a local REPL where you can talk to the agent directly.

Head to [Your First Session](./first-session) for what to try next.

## Installing just the CLI

If you want the `exo` CLI without the canonical agent — to build your own
harness from scratch or script against the exoharness — install it from a
checkout with cargo:

```bash
git clone https://github.com/exoharness/exo
cd exo
cargo install --path crates/cli --locked
exo --help
```

This places a release build at `~/.cargo/bin/exo` (on your `PATH` via
rustup). See [Using the CLI Directly](./quick-start) to register a model
and start a bare REPL, and run `pnpm install` if you'll use TypeScript
harnesses.

::: info
  Hacking on exo itself? Use a debug build: `cargo build -p exo`, then invoke
  it as `./target/debug/exo`.
:::
