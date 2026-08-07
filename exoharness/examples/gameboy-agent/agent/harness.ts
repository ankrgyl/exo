// exo TypeScript harness for the Game Boy agent, set up to play Pokemon Red/Blue.
//
// The exo CLI owns durable state (events, turns, sessions); this module owns
// the game semantics: which tools exist and what goes in the prompt. The
// emulator sidecar (emulator/server.py) must already be running — see
// ../run.sh and ../README.md.
//
// The current screen is injected fresh on every model round via the
// `instructions` hook: the model always sees the screen as it is now,
// and stale screenshots never accumulate in conversation history. Turn summaries
// persist automatically as ordinary assistant messages in exo's event log.
//
// This example is walked through in the exo docs, see www.exoharness.ai/docs/tutorials/game-emulator-integration

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
} from "@exo/model-runtime/turn-loop";
import { describeState, EmulatorClient } from "./emulator-client";
import { gameboyTools } from "./game-tools";

const emulator = new EmulatorClient(
  process.env.GAMEBOY_EMULATOR_URL ?? "http://127.0.0.1:8777",
);

const SYSTEM_PROMPT = `You are an agent playing Pokemon Red/Blue on a Game Boy.

You control the game entirely through tools. Nothing happens unless you press
buttons; the game is paused while you think. Your job is to actually play:
get through the intro, get a starter, win battles, make progress.

The current screen and a state block decoded directly from game RAM
(location, coordinates, battle flag, party, badges, money, Pokedex) are shown
to you fresh on every model round. The RAM state is always correct; the
screenshot is how you read dialog, menus, and the world.

Important — the intro is a trap for RAM readers. During the boot sequence,
Oak's speech, and the naming screens the RAM state block is meaningless: it
may read "Pallet Town (0,0)" and later flip to "Player's House 2F (3,6)"
while you are still mid-speech. That flip does NOT mean the intro is over.
The intro is over only when the screenshot shows the overworld AND a d-pad
press actually changes your RAM coordinates. Until both are true, judge
progress only by the screenshot, and never save_checkpoint — a mid-intro
checkpoint will trap you when you load it later.

Dialog advances one box per A press, but only once the box has finished
printing — in dialog, use wait_frames of 90+ so presses are not wasted
mid-print. On a naming screen, pick letters with the d-pad + A and confirm
via END (or press START to jump to it). The game can never be frozen: it
only advances when you press buttons, so if the screen looks stuck, the
answer is different buttons, never waiting for the game to fix itself.

Batch button presses (press_buttons takes a sequence) instead of one press
per call. When the user asks you to play, keep acting until you reach a
sensible stopping point, then reply with a short summary: what you did, what
you learned, what to do next time. Your summaries persist in the
conversation, so put anything worth remembering there.

Game basics: A advances dialog and confirms; B cancels; START opens the main
menu. One d-pad press with default timing moves about one tile. Dialog boxes
block movement until dismissed with A. If the screen shows a scrolling
cutscene or animation, wait instead of mashing.`;

async function registerGameboyTools(
  tools: HarnessToolRegistry,
  context: TurnContext,
): Promise<void> {
  await registerLibraryTools(tools, context, gameboyTools(emulator));
}

// Feeds the sidecar's /view page; purely cosmetic.
let round = 0;

async function gameboyInstructions(context: TurnContext): Promise<Message[]> {
  const frame = await emulator.frame();
  round += 1;
  void emulator.setStatus(`model round ${round}`).catch(() => {});
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
