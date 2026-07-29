---
title: Game & Third-Party Tool Integration
description: Wire an agent up to a game or any external stateful system with a sidecar API, ground-truth state, and a small tool surface — shown end to end with a Game Boy emulator.
---

# Game & Third-Party Tool Integration

![An exo agent playing Pokemon Red, watched live in the sidecar's /view page](/images/gameboy-agent-view.png)

This tutorial shows how to connect an agent built in the exoharness framework to an external system, with the motivating example being a Game Boy playing Pokemon Red.

The finished code is in [`examples/gameboy-agent`](https://github.com/exoharness/exo/tree/main/examples/gameboy-agent): a ~200-line Python sidecar, a ~100-line TypeScript client, five tools, and a
~90-line exo harness.

![Architecture: the model talks to the exo turn loop and harness, which drives the emulator sidecar over localhost HTTP](/images/gameboy-agent-architecture.svg)

This builds directly on the [Custom Agent Quickstart](./write-your-own-agent);
if you haven't written an exo harness before, start there.

## Step 1: Running the emulator

In this tutorial we use the open source Game Boy emulator, [PyBoy](https://github.com/Baekalfen/PyBoy). It is written in Python, and runs in its own standalone process shown in
[`emulator/server.py`](https://github.com/exoharness/exo/blob/main/examples/gameboy-agent/emulator/server.py), exposing the following endpoints over localhost HTTP:

| Endpoint | Purpose |
| --- | --- |
| `POST /press {buttons, hold_frames?, wait_frames?}` | act |
| `POST /tick {frames}` | let time pass (cutscenes, animations) |
| `GET /frame` | observe without acting |
| `POST /checkpoint/save` / `load`, `GET /checkpoints` | snapshot / rewind |
| `GET /health`, `POST /reset` | plumbing |

This emulator is a good environment for an agent to run in because the agent drives the speed at which the environment progresses with `/press` and `/tick`. Between requests, the game stays fully frozen, so is fully turn-based.

The payload carries two views of the game:
- **A screenshot**, 3x upscaled with hard edges for readability.
- **Ground-truth state decoded from RAM.** Emulators expose the game's memory, and community symbol maps
  exist for most classics — this example uses the
  [pret/pokered disassembly](https://github.com/pret/pokered), with the
  [Data Crystal RAM map](https://datacrystal.tcrf.net/wiki/Pok%C3%A9mon_Red_and_Blue/RAM_map)
  as a readable cross-reference.
  [`emulator/memory_map.py`](https://github.com/exoharness/exo/blob/main/examples/gameboy-agent/emulator/memory_map.py)
  reads a dozen WRAM addresses and returns facts:

```json
{
  "map_name": "Viridian Forest",
  "x": 17,
  "y": 24,
  "facing": "up",
  "in_battle": "wild",
  "party": [{ "species": "Squirtle", "level": 9, "hp": 21, "max_hp": 26 }]
}
```

## Step 2: Wrapping the emulator in a client

[`agent/emulator-client.ts`](https://github.com/exoharness/exo/blob/main/examples/gameboy-agent/agent/emulator-client.ts)
wraps the HTTP API in a typed client — one method per endpoint, deliberately
boilerplate. It exists so the tools and harness never touch HTTP or JSON
shapes directly:

```ts
export interface GameState {
  map_id: number;
  map_name: string;
  x: number;
  y: number;
  facing: string;
  in_battle: "none" | "wild" | "trainer" | "lost";
  badges: string[];
  badge_count: number;
  money: number;
  party: PartyMon[];
  pokedex_owned: number;
}

export interface FramePayload {
  screenshot_b64: string;
  state: GameState;
  screen_hash: string;
  frame_count: number;
  status: string;
}

export class EmulatorClient {
  constructor(private readonly baseUrl: string) {}

  async frame(): Promise<FramePayload> {
    return await this.request("GET", "/frame");
  }

  async press(
    buttons: string[],
    holdFrames?: number | null,
    waitFrames?: number | null,
  ): Promise<FramePayload> {
    return await this.request("POST", "/press", {
      buttons,
      hold_frames: holdFrames ?? undefined,
      wait_frames: waitFrames ?? undefined,
    });
  }

  // ...tick, saveCheckpoint, loadCheckpoint, listCheckpoints: same shape

  private async request<T>(
    method: string,
    path: string,
    body?: unknown,
  ): Promise<T> {
    const response = await fetch(`${this.baseUrl}${path}`, {
      method,
      headers: body === undefined ? {} : { "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(60_000),
    });
    const payload = (await response.json()) as T & { error?: string };
    if (!response.ok) {
      throw new Error(
        `emulator ${method} ${path} failed (${response.status}): ${payload?.error ?? "unknown error"}`,
      );
    }
    return payload;
  }
}
```

The client also exports `describeState(state)`, which renders the RAM state
into the compact text block that tool results and the prompt both use
(`location: Viridian Forest (map 0x33) at (17,24) facing up` …).

## Step 3: Exposing the client via tooling

[`agent/game-tools.ts`](https://github.com/exoharness/exo/blob/main/examples/gameboy-agent/agent/game-tools.ts)
turns the client into exo `Tool` objects — a strict-schema `definition` the
model sees, and an `initialize()` returning the handler that runs when the
model calls it. Here is the main one in full:

```ts
import { defineTool, type Tool } from "@exo/harness";
import { describeState, type EmulatorClient } from "./emulator-client";

const BUTTONS = ["a", "b", "start", "select", "up", "down", "left", "right"];

const NO_PARAMETERS = {
  type: "object",
  additionalProperties: false,
  properties: {},
} as const;

export function gameboyTools(emulator: EmulatorClient): Tool[] {
  return [
    defineTool({
      definition: {
        name: "press_buttons",
        description:
          "Press a sequence of Game Boy buttons, one after another. Each button is held for hold_frames then released, followed by wait_frames of settle time (60 frames = 1 second). One d-pad press with default timing moves the player roughly one tile. Returns the resulting RAM-derived game state; the refreshed screen appears in your next model round.",
        parameters: {
          type: "object",
          additionalProperties: false,
          properties: {
            buttons: {
              type: "array",
              items: { type: "string", enum: BUTTONS },
              description: "1-20 buttons pressed in order.",
            },
            hold_frames: {
              type: ["number", "null"],
              description:
                "Frames to hold each button (default 10, max 120). Longer holds walk further per press.",
            },
            wait_frames: {
              type: ["number", "null"],
              description:
                "Frames to wait after each release (default 45, max 600). Increase when animations or dialog need time.",
            },
          },
          required: ["buttons", "hold_frames", "wait_frames"],
        },
      },
      initializationParameters: NO_PARAMETERS,
      initialize() {
        return {
          async execute(args) {
            const frame = await emulator.press(
              args.buttons.map(String),
              numberOrNull(args.hold_frames),
              numberOrNull(args.wait_frames),
            );
            return `pressed [${args.buttons.join(", ")}]\n${describeState(frame.state)}`;
          },
        };
      },
    }),
    // ...wait, save_checkpoint, load_checkpoint, list_checkpoints
  ];
}
```

The other four tools follow the same pattern.

- **Batch actions per call.** `press_buttons` takes a *sequence* to help minimize costs (allows multiple "steps" without more intermediate processing).
- **An explicit `wait`.** Helps avoid mashing cutscenes.
- **Snapshot and rewind** (`save_checkpoint` / `load_checkpoint`).

Note the tools return *text only* — the `describeState` rendering. The
screenshot is delivered separately, by the harness, so stale frames never
accumulate in history (next step).

## Step 4: Constructing the agent

[`agent/harness.ts`](https://github.com/exoharness/exo/blob/main/examples/gameboy-agent/agent/harness.ts)
is the whole agent policy written as an exoharness typescript executor.

```ts
import {
  defineHarness,
  registerLibraryTools,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import {
  basicHarnessInstructions,
  runResponsesHarnessTurn,
} from "../../typescript/turn-loop";
import { describeState, EmulatorClient } from "./emulator-client";
import { gameboyTools } from "./game-tools";

const emulator = new EmulatorClient(
  process.env.GAMEBOY_EMULATOR_URL ?? "http://127.0.0.1:8777",
);

const SYSTEM_PROMPT = `You are an agent playing Pokemon Red/Blue on a Game Boy.
...game rules: how turns work, dialog timing, intro pitfalls, button basics...`;

async function registerGameboyTools(
  tools: HarnessToolRegistry,
  context: TurnContext,
): Promise<void> {
  await registerLibraryTools(tools, context, gameboyTools(emulator));
}

async function gameboyInstructions(context: TurnContext): Promise<Message[]> {
  const frame = await emulator.frame();
  return [
    ...basicHarnessInstructions(context),
    { role: "developer", content: SYSTEM_PROMPT },
    {
      role: "user",
      content: [
        {
          type: "text",
          text: `Current Game Boy screen (refreshed for this model round):\n${describeState(frame.state)}`,
        },
        {
          type: "image",
          image: frame.screenshot_b64,
          media_type: "image/png",
        },
      ],
    },
  ];
}

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions: gameboyInstructions,
      registerTools: registerGameboyTools,
    });
  },
});
```

The `instructions` hook is where the logic to provide screenshots: it fetches the live screen with `emulator.frame()` and returns it as a `text` + `image` user message — the freshest screenshot is spliced in each round rather than accumulated, so the model always acts on the screen as it is *now*. `maxToolRoundTrips` on the agent config bounds tool calls per turn.

## Step 5: Running the agent

Now, to run our system, there are two processes of interest: the emulator sidecar, and exo driving turns against it.
[`run.sh`](https://github.com/exoharness/exo/blob/main/examples/gameboy-agent/run.sh) starts the sidecar (a venv with PyBoy is created on first run):

```bash
#!/usr/bin/env bash
# Boots the PyBoy emulator sidecar (foreground).
set -euo pipefail
cd "$(dirname "$0")"

ROM=$(ls roms/*.gb roms/*.gbc 2>/dev/null | head -1 || true)
# ...ROM + venv checks elided...

PORT="${GAMEBOY_EMULATOR_PORT:-8777}"
HOST="${GAMEBOY_EMULATOR_HOST:-127.0.0.1}"
exec "$VENV/bin/python" emulator/server.py --rom "$ROM" --port "$PORT" --host "$HOST"
```

```bash
cd examples/gameboy-agent
mkdir -p roms && cp /path/to/pokemon-red.gb roms/   # ROMs are copyrighted — bring your own
./run.sh                       # boots PyBoy on http://127.0.0.1:8777
```

Leave it running and open <http://127.0.0.1:8777/view> — a minimal page
served by the sidecar itself showing the live screen, the current model
round, and the RAM state, refreshing once a second.

Then, in another terminal (from the repo root), create the agent and play:

```bash
exo secret set openai --env OPENAI_API_KEY          # once
exo model register gpt-5.5 --secret openai          # once

exo --harness typescript agent create "Gameboy" \
  --module examples/gameboy-agent/agent/harness.ts \
  --model gpt-5.5 --max-tool-round-trips 20         # once
exo conversation create gameboy "Play Pokemon"      # once

exo conversation send gameboy play-pokemon \
  "Play Pokemon Red. Get through the intro and pick a starter."
```

Each `send` runs one exo turn — the model presses buttons until it reaches a
stopping point (or the round-trip cap), its summary lands in the
conversation, and the full turn transcript prints. Send again to continue;
prior summaries are already in history. To just let it play, iterate:

```bash
for i in $(seq 1 25); do
  exo conversation send gameboy play-pokemon \
    "Continue playing. Make real progress; end with a one-line summary."
done
```

See the
[example README](https://github.com/exoharness/exo/tree/main/examples/gameboy-agent)
for the remaining knobs (`GAMEBOY_EMULATOR_URL`, ports, fresh starts).

## Adapting the pattern

For a different Game Boy game, replace `memory_map.read_state` (or return
`{}` and play vision-only), adjust the `GameState` type, and rewrite the
system prompt — everything else is game-agnostic. For another console, keep
the API shape and swap the core (NES/SNES/GBA cores, ScummVM, DOSBox all
script the same way).

For a third-party tool that isn't a game, the checklist is the same steps:

1. Wrap it in a sidecar in its native ecosystem; freeze time between
   requests if it has a clock; return the post-action observation from
   every mutating endpoint.
2. Find the ground-truth state channel (SDK, debug port, database) and
   return decoded facts next to the raw observation.
3. Shape tools for the model's budget: batched actions, an explicit wait,
   snapshot/rewind if the system allows it.
4. Inject the live observation in the harness's `instructions` hook so every
   model round sees current state instead of accumulating stale snapshots.

## Going further: self-improvement

This example is the minimal skeleton. The evaluation it was extracted from
layers self-improvement on the same pieces: a playbook the agent
rewrites and re-reads every turn, durable memory files, and an
`install_tool` meta-tool that lets the agent write new ES-module tools
composing the emulator primitives. Over two long runs the agent authored 14
tools (dialog mashing, battle recovery, pathing macros) and 2 skills
entirely on its own. 

## Appendix: extending an existing agent instead

Everything above builds a **new** agent executor: `harness.ts` is its own
module, registered with `agent create`. But the harness contract — hooks
that return data (message lists, tool registrations) — means you can also
graft the game layer onto an agent you already run, by composition. The
canonical Exo agent exports its two hooks for exactly this:

```ts
// exo-gameboy-harness.ts — the Exo agent, decorated with the game layer
import {
  defineHarness,
  registerLibraryTools,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import { runResponsesHarnessTurn } from "../../typescript/turn-loop";
import { exoInstructions, registerExoTools } from "../../exo/harness";
import { EmulatorClient, describeState } from "./emulator-client";
import { gameboyTools } from "./game-tools";

const emulator = new EmulatorClient(
  process.env.GAMEBOY_EMULATOR_URL ?? "http://127.0.0.1:8777",
);

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      // Exo's tools + the game tools, one registry.
      async registerTools(
        tools: HarnessToolRegistry,
        ctx: TurnContext,
        model: string,
      ) {
        await registerExoTools(tools, ctx, model);
        await registerLibraryTools(tools, ctx, gameboyTools(emulator));
      },

      // Exo's identity/memory/skills prompt + the live screen each round.
      async instructions(ctx: TurnContext): Promise<Message[]> {
        const frame = await emulator.frame();
        return [
          ...(await exoInstructions(ctx)),
          {
            role: "user",
            content: [
              {
                type: "text",
                text: `Current Game Boy screen (refreshed each round):\n${describeState(frame.state)}`,
              },
              {
                type: "image",
                image: frame.screenshot_b64,
                media_type: "image/png",
              },
            ],
          },
        ];
      },
    });
  },
});
```

Point your **existing** agent record at the wrapper and it keeps its memory,
artifacts, skills, and every conversation — it just gains a live view of the emulator, and tooling to press buttons:

```bash
exo agent update exo --module path/to/exo-gameboy-harness.ts
# ...and back again to un-extend:
exo agent update exo --module examples/exo/harness.ts
```
