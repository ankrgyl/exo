// Tools for interacting with the Game Boy emulator. Text results carry the
// RAM-decoded state; the harness's instructions hook supplies the screenshot
// each model round, so tools never need to return images.

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
            const buttons = args.buttons;
            if (
              !Array.isArray(buttons) ||
              buttons.some((button) => !BUTTONS.includes(String(button)))
            ) {
              return `buttons must be an array of ${BUTTONS.join("/")}`;
            }
            const frame = await emulator.press(
              buttons.map(String),
              numberOrNull(args.hold_frames),
              numberOrNull(args.wait_frames),
            );
            return `pressed [${buttons.join(", ")}]\n${describeState(frame.state)}`;
          },
        };
      },
    }),
    defineTool({
      definition: {
        name: "wait",
        description:
          "Let the game run for N frames without pressing anything (60 frames = 1 second). Use when a cutscene, animation, or dialog is still playing.",
        parameters: {
          type: "object",
          additionalProperties: false,
          properties: {
            frames: {
              type: "number",
              description: "Frames to advance (max 3600).",
            },
          },
          required: ["frames"],
        },
      },
      initializationParameters: NO_PARAMETERS,
      initialize() {
        return {
          async execute(args) {
            const frames = Number(args.frames);
            if (!Number.isFinite(frames) || frames < 1) {
              return "frames must be a positive number";
            }
            const frame = await emulator.tick(
              Math.min(Math.round(frames), 3600),
            );
            return `waited ${frames} frames\n${describeState(frame.state)}`;
          },
        };
      },
    }),
    defineTool({
      definition: {
        name: "save_checkpoint",
        description:
          "Save an emulator save-state under a name so you can rewind to this exact moment later with load_checkpoint. Save before risky sections (gym fights, long routes).",
        parameters: {
          type: "object",
          additionalProperties: false,
          properties: {
            name: {
              type: "string",
              description: "Checkpoint name, [A-Za-z0-9_.-]{1,64}.",
            },
          },
          required: ["name"],
        },
      },
      initializationParameters: NO_PARAMETERS,
      initialize() {
        return {
          async execute(args) {
            await emulator.saveCheckpoint(String(args.name ?? ""));
            return `checkpoint '${args.name}' saved`;
          },
        };
      },
    }),
    defineTool({
      definition: {
        name: "load_checkpoint",
        description:
          "Rewind the game to a previously saved checkpoint. Use when you are wedged (blacked out, softlocked in a menu, walked far off course).",
        parameters: {
          type: "object",
          additionalProperties: false,
          properties: {
            name: { type: "string", description: "Checkpoint name to load." },
          },
          required: ["name"],
        },
      },
      initializationParameters: NO_PARAMETERS,
      initialize() {
        return {
          async execute(args) {
            const frame = await emulator.loadCheckpoint(
              String(args.name ?? ""),
            );
            return `rewound to checkpoint '${args.name}'\n${describeState(frame.state)}`;
          },
        };
      },
    }),
    defineTool({
      definition: {
        name: "list_checkpoints",
        description: "List all saved checkpoint names.",
        parameters: NO_PARAMETERS,
      },
      initializationParameters: NO_PARAMETERS,
      initialize() {
        return {
          async execute() {
            const names = await emulator.listCheckpoints();
            return names.length === 0 ? "no checkpoints yet" : names.join("\n");
          },
        };
      },
    }),
  ];
}

function numberOrNull(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}
