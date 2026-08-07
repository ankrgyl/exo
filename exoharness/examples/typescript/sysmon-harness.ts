// Tutorial harness: a small "system monitor" agent with a dedicated tool and
// a custom context-building step that injects live host memory utilization
// into the prompt on every model round. See docs-site tutorial
// "Write Your Own Agent".

import os from "node:os";

import {
  defineHarness,
  defineTool,
  registerBuiltInTools,
  registerLibraryTools,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import {
  basicHarnessInstructions,
  runResponsesHarnessTurn,
} from "@exo/model-runtime/turn-loop";

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

async function registerSysmonTools(
  tools: HarnessToolRegistry,
  context: TurnContext,
): Promise<void> {
  // Existing tools that ship with exo. shell runs commands in the sandbox;
  // install_agent_tool / uninstall_agent_tool let the agent write and remove
  // its own tools at runtime.
  registerBuiltInTools(tools, context, [
    "shell",
    "install_agent_tool",
    "uninstall_agent_tool",
  ]);
  // Our custom tool.
  await registerLibraryTools(tools, context, systemInfoTool);
}

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

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions: sysmonInstructions,
      registerTools: registerSysmonTools,
    });
  },
});
