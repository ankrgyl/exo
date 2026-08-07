---
title: Custom Agent Quickstart
description: Build a TypeScript harness that reuses built-in tools, adds a custom tool, and customizes context.
---

# Custom Agent Quickstart

In this tutorial you'll build a small "system monitor" agent as a single
TypeScript harness module. It covers the three things most custom agents
need:

1. **Reuse tools that ship with exo** — register the built-in `shell` tool
   (and its siblings) in one call.
2. **Add a custom tool** — a `system_info` tool the model can call.
3. **Customize context building** — the host's live memory utilization is
   injected into the prompt on *every model round*, so the agent always knows
   the current number without calling a tool.

The finished file lives at
[`exoharness/examples/typescript/sysmon-harness.ts`](https://github.com/exoharness/exo/blob/main/exoharness/examples/typescript/sysmon-harness.ts).

## The harness contract

A TypeScript harness is a module whose default export implements one method:
`runTurn(context)`. The Rust CLI owns durable state (events, artifacts,
secrets, sandboxes); your module owns everything semantic — what goes in the
prompt, which tools exist, when the turn is done.

The smallest possible harness delegates the whole loop to the stock
Responses-API turn loop:

```ts
import { defineHarness } from "@exo/harness";
import { runResponsesHarnessTurn } from "./turn-loop";

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context);
  },
});
```

That's [`exoharness/examples/typescript/basic-harness.ts`](https://github.com/exoharness/exo/blob/main/exoharness/examples/typescript/basic-harness.ts)
verbatim. `runResponsesHarnessTurn` accepts two hooks, and they are the whole
customization surface for this tutorial:

- `instructions(context)` — return the instruction messages for each model
  round.
- `registerTools(tools, context)` — populate the tool registry for each model
  round.

## Step 1: reuse tools that ship with exo

When you pass a `registerTools` hook, you own the whole registry — so
built-in tools are opt-in, and you add the ones you want in a single call.
exo ships three built-ins:

- `shell` — run commands in the conversation's sandbox.
- `install_agent_tool` / `uninstall_agent_tool` — let the agent write and
  remove its own TypeScript tools at runtime (they land in
  `.exo/agent-tools/`). Using these well also needs the agent-tool-creation
  instruction; see [The Canonical Agent](../concepts/canonical-agent).

```ts
import {
  registerBuiltInTools,
  type HarnessToolRegistry,
  type TurnContext,
} from "@exo/harness";

async function registerSysmonTools(
  tools: HarnessToolRegistry,
  context: TurnContext,
): Promise<void> {
  registerBuiltInTools(tools, context, [
    "shell",
    "install_agent_tool",
    "uninstall_agent_tool",
  ]);
  // ...custom tools go here (Step 2)
}
```

The registry is rebuilt every model round, so a tool the agent installs
mid-turn via `install_agent_tool` is visible in the very next round.

## Step 2: add a custom tool

Tools are plain objects: a JSON-schema `definition` the model sees, and an
`initialize()` that returns the handler which runs when the model calls it.

```ts
import os from "node:os";
import { defineTool } from "@exo/harness";

const systemInfoTool = defineTool({
  definition: {
    name: "system_info",
    description:
      "Report host platform, CPU count, load average, and uptime. Use it when asked about the machine this agent runs on.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {},
    },
  },
  initializationParameters: {
    type: "object",
    additionalProperties: false,
    properties: {},
  },
  initialize() {
    return {
      async execute() {
        return {
          platform: `${os.platform()} ${os.release()} (${os.arch()})`,
          cpus: os.cpus().length,
          loadAverage: os.loadavg().map((load) => Number(load.toFixed(2))),
          uptimeSeconds: Math.round(os.uptime()),
        };
      },
    };
  },
});
```

Notes:

- `parameters` must be a strict JSON schema (`additionalProperties: false`).
  This tool takes no arguments; `execute(args, execution)` receives them when
  it does. `execution.context` is the full `TurnContext`, so tools can run
  sandbox commands, read events, or write artifacts.
- `initializationParameters` / `initialize(args)` exist so a tool can be
  configured per-agent (an API prefix, a base URL) separately from per-call
  arguments — see
  [`exoharness/examples/typescript/tools/uppercase.ts`](https://github.com/exoharness/exo/blob/main/exoharness/examples/typescript/tools/uppercase.ts)
  for a configured example. Ours needs no configuration.
- Harness code runs on the **host**, so `node:os` reports the host machine.
  For work that belongs in isolation, call the `shell` tool or
  `context.startSandboxProcess(...)` instead.

Register it alongside the built-ins with `registerLibraryTools`, filling in
the placeholder from Step 1:

```ts
import { registerLibraryTools } from "@exo/harness";

// inside registerSysmonTools, after registerBuiltInTools(...):
await registerLibraryTools(tools, context, systemInfoTool);
```

## Step 3: customize context building

The `instructions` hook runs before every model round and returns the
messages prepended to the prompt. Because it re-runs each round, it's the
place to inject live context — here, the host's current memory usage:

```ts
import { type Message } from "@exo/harness";
import { basicHarnessInstructions } from "./turn-loop";

function sysmonInstructions(context: TurnContext): Message[] {
  const totalBytes = os.totalmem();
  const usedBytes = totalBytes - os.freemem();
  const usedPercent = ((usedBytes / totalBytes) * 100).toFixed(1);
  return [
    ...basicHarnessInstructions(context),
    {
      role: "developer",
      content:
        `Host memory utilization right now: ${usedPercent}% ` +
        `(${gibibytes(usedBytes)} GiB of ${gibibytes(totalBytes)} GiB in use). ` +
        "This is measured fresh for every model round, so treat it as current.",
    },
  ];
}

function gibibytes(bytes: number): string {
  return (bytes / 1024 ** 3).toFixed(1);
}
```

Because the hook re-runs each round, a turn with five tool round-trips sees
the memory number as of each call, not frozen at the turn's start. (These
injected messages are rebuilt every round rather than stored in the
conversation history — see [Data Model](../concepts/data-model) if you
want the details on why.)

The same pattern scales to anything derivable: the canonical agent's harness
([`exo/harness.ts`](https://github.com/exoharness/exo/blob/main/exo/harness.ts))
uses it to splice in an identity prompt, a git-ignored local profile file,
and the agent's persistent memory block each round.

## Step 4: put it together

```ts
export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions: sysmonInstructions,
      registerTools: registerSysmonTools,
    });
  },
});
```

## Run it

```bash
pnpm install   # once, for the TypeScript runtime

exo secret set openai --env OPENAI_API_KEY
exo model register gpt-5.5 --secret openai

exo --harness typescript agent create "Sysmon" \
  --module exoharness/examples/typescript/sysmon-harness.ts \
  --model gpt-5.5
exo conversation create sysmon "Sysmon Test"
exo repl --agent sysmon --conversation sysmon-test
```

A real exchange (via `conversation send`):

```text
user: What is your host's current memory utilization? Then call
      system_info and summarize the result in one line.
assistant: [tool_call system_info] {}
tool system_info: { "platform": "linux 6.8.0-124-generic (x64)",
                    "cpus": 14, "loadAverage": [0.23, 0.14, 0.19],
                    "uptimeSeconds": 482219 }
assistant: Host memory utilization: 36.0% (8.4 GiB of 23.4 GiB in use).
           System info: linux 6.8.0-124-generic (x64), 14 CPUs, load
           average 0.23/0.14/0.19, uptime 482,219 seconds.
```

The memory figure came straight from the injected developer message — no
tool call needed — while the platform details came from the `system_info`
tool.

::: info
  Tool results are stored as versioned artifacts automatically. The model sees
  a preview inline (`ok`, `preview`, `value`), and the full result stays
  retrievable from the artifact store — another place durable state quietly
  does its job.
:::

## Where to go next

- Give a tool per-agent configuration with `initializationParameters`
  (see `tools/uppercase.ts`).
- Ship tools as separate modules and load them with
  `--tool-module <path>` / `toolModulePaths` instead of defining them inline.
- Let the agent create its own tools at runtime by enabling
  `enableAgentToolCreation` — the built-in `install_agent_tool` writes tool
  modules to `.exo/agent-tools/`.
- Read [Executors & Harnesses](../concepts/executors) for how this
  module relates to the exoharness underneath it.
