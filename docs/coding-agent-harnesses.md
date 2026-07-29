# Coding Agent Harnesses

The Codex, Claude Code, and Cursor examples treat exoharness events as canonical
conversation state and run their native agent runtimes inside configured
exoharness sandboxes.

Install dependencies and build the CLI first:

```bash
pnpm install
cargo build -p exo
```

The examples below use `./target/debug/exo`. If you have the binary on your
`PATH`, you can use `exo` instead.

The `codex`, `claude-code`, and `cursor` harness presets select the matching
TypeScript module, sandbox image, and networking defaults.

For `secret set`, `--env` takes the variable name literally. For example, use
`--env OPENAI_API_KEY`, not `--env $OPENAI_API_KEY`.

The sandbox image commands use Apple container. It currently requires an Apple
silicon Mac running macOS 26 or newer.

Install Apple container:

1. Download the latest signed installer package from
   <https://github.com/apple/container/releases>.
2. Open the package and follow the installer prompts. It installs files under
   `/usr/local` and may ask for an administrator password.
3. Start the container system service:

```bash
container system start
```

For upgrades, downgrades, uninstall instructions, and building from source, see
<https://github.com/apple/container>.

## Codex

Sign Exo in with ChatGPT, then register a secretless model binding. This uses
the Codex access included with your ChatGPT plan rather than API billing:

```bash
./target/debug/exo codex login
./target/debug/exo model register gpt-5.5
```

The Exo login is isolated from the Codex CLI and VS Code extension. Use
`exo codex status` to inspect it, `exo codex logout` to remove it, and
`exo codex login --device-auth` on a headless machine. Set `EXO_CODEX_HOME`
to override Exo's platform-specific credential directory.

To use API billing instead, register the model with an API-key secret:

```bash
./target/debug/exo secret set openai --env OPENAI_API_KEY
./target/debug/exo model register gpt-5.5 --secret openai
```

ChatGPT subscription authentication is available only to the Codex harness.
The basic, RLM, and direct Responses API harnesses still require API credentials.

Build the sandbox image:

```bash
docker build \
  --platform linux/arm64 \
  -t exo-codex-sandbox:latest \
  containers/codex-sandbox
```

To launch the normal Exo stack with the Codex harness, including Docker,
ExoChat, the adapter runner, and the CLI chat, run:

```bash
./exo.sh --harness codex
```

The Codex harness owns the model turn and sandbox tool loop. Exo still owns
agent and conversation state, sandbox lifecycle, adapter persistence, inbound
wake-ups, and ExoChat delivery. The Codex preset currently exposes Exo's
adapter tools; it does not combine the Codex loop with every tool and prompt
from `examples/exo/harness.ts`.

Create the agent and start a conversation:

```bash
./target/debug/exo --harness codex agent create "TS Codex" \
  --model gpt-5.5

./target/debug/exo conversation create ts-codex
./target/debug/exo conversation mount add ts-codex <conversation> "$PWD" /workspace --rw
./target/debug/exo repl --agent ts-codex --conversation <conversation>
```

## Claude Code

Register an Anthropic model:

```bash
./target/debug/exo secret set anthropic --env ANTHROPIC_API_KEY
./target/debug/exo model register claude-sonnet-4-6 --secret anthropic
```

Build the sandbox image:

```bash
container build \
  --platform linux/arm64 \
  -t exo-claude-code-sandbox:latest \
  containers/claude-code-sandbox
```

Create the agent and start a conversation:

```bash
./target/debug/exo --harness claude-code agent create "TS Claude Code" \
  --model claude-sonnet-4-6

./target/debug/exo conversation create ts-claude-code
./target/debug/exo conversation mount add ts-claude-code <conversation> "$PWD" /workspace --rw
./target/debug/exo repl --agent ts-claude-code --conversation <conversation>
```

## Cursor

Register a Cursor model:

```bash
./target/debug/exo secret set cursor --env CURSOR_API_KEY
./target/debug/exo model register auto --secret cursor
```

Build the sandbox image:

```bash
container build \
  --platform linux/arm64 \
  -f containers/cursor-sdk-sandbox/Containerfile \
  -t exo-cursor-sdk-sandbox:latest \
  .
```

Create the agent and start a conversation:

```bash
./target/debug/exo --harness cursor agent create "TS Cursor" \
  --model auto

./target/debug/exo conversation create ts-cursor
./target/debug/exo conversation mount add ts-cursor <conversation> "$PWD" /workspace --rw
./target/debug/exo repl --agent ts-cursor --conversation <conversation>
```

## Live E2E

The live e2e script runs replay checks against the coding-agent harnesses:

```bash
pnpm e2e:agent-harnesses --only codex
pnpm e2e:agent-harnesses --only claude
pnpm e2e:agent-harnesses --only cursor
```

Use `--build-images` to build the required sandbox images before running.
