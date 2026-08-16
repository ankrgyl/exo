# Coding Agent Harnesses

The Codex, Claude Code, Cursor, and Pi examples treat exoharness events as
canonical conversation state and run their native agent runtimes inside
configured exoharness sandboxes.

Install dependencies and build the CLI first:

```bash
pnpm install
cargo build -p exo
```

The examples below use `./target/debug/exo`. If you have the binary on your
`PATH`, you can use `exo` instead.

The `codex`, `claude-code`, `cursor`, and `pi` harness presets select the
matching TypeScript module, sandbox image, and networking defaults.

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

Register an OpenAI model:

```bash
./target/debug/exo secret set openai --env OPENAI_API_KEY
./target/debug/exo model register gpt-5.5 --secret openai
```

Build the sandbox image:

```bash
container build \
  --platform linux/arm64 \
  -t exo-codex-sandbox:latest \
  exoharness/containers/codex-sandbox
```

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
  exoharness/containers/claude-code-sandbox
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
  -f exoharness/containers/cursor-sdk-sandbox/Containerfile \
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

## Pi

Register a Pi model using its `provider/model` form:

```bash
./target/debug/exo secret set pi-anthropic --env ANTHROPIC_API_KEY
./target/debug/exo model register pi-claude \
  --model anthropic/claude-sonnet-4-6 \
  --secret pi-anthropic
```

Build the sandbox image from the repository root:

```bash
container build \
  --platform linux/arm64 \
  -f exoharness/containers/pi-sandbox/Containerfile \
  -t exo-pi-sandbox:latest \
  .
```

Create the agent and start a conversation:

```bash
./target/debug/exo --harness pi agent create "TS Pi" \
  --model pi-claude

./target/debug/exo conversation create pi
./target/debug/exo conversation mount add pi <conversation> "$PWD" /workspace --rw
./target/debug/exo repl --agent pi --conversation <conversation>
```

Like the other coding-agent harnesses, Pi runs inside the configured sandbox
and treats exoharness events as canonical history. Each turn uses an in-memory
Pi session, so no separate Pi session is persisted. The worker does not load
ambient `~/.pi` or project extensions, skills, prompts, or context files.

The preset enables networking because Pi makes model requests in the sandbox.
It currently supports Pi's built-in model catalog; an Exo `--base-url` overrides
the selected model's endpoint.

## Live E2E

The live e2e script runs replay checks against the coding-agent harnesses:

```bash
pnpm e2e:agent-harnesses --only codex
pnpm e2e:agent-harnesses --only claude
pnpm e2e:agent-harnesses --only cursor
pnpm e2e:agent-harnesses --only pi
```

Use `--build-images` to build the required sandbox images before running.
