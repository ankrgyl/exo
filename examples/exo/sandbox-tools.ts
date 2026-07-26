import type {
  HarnessToolRegistry,
  ToolDefinition,
  ToolInstance,
} from "@exo/harness";

import { hostTool } from "./host-tools";

export type SandboxToolName =
  | "get_sandbox_status"
  | "list_sandbox_snapshots"
  | "snapshot_sandbox"
  | "rewind_sandbox";

export function registerSandboxTools(
  registry: HarnessToolRegistry,
  names: SandboxToolName[] = [
    "get_sandbox_status",
    "list_sandbox_snapshots",
    "snapshot_sandbox",
    "rewind_sandbox",
  ],
): void {
  const requested = new Set<SandboxToolName>(names);
  for (const tool of createSandboxToolInstances()) {
    if (requested.has(tool.definition.name as SandboxToolName)) {
      registry.register(tool);
    }
  }
}

function createSandboxToolInstances(): ToolInstance[] {
  return [
    getSandboxStatusTool(),
    listSandboxSnapshotsTool(),
    snapshotSandboxTool(),
    rewindSandboxTool(),
  ];
}

function getSandboxStatusTool(): ToolInstance {
  return hostTool({
    name: "get_sandbox_status",
    description:
      "Inspect whether the selected Exo sandbox exists and report its id and configured scope without starting it.",
    parameters: scopeParameters(),
  });
}

function listSandboxSnapshotsTool(): ToolInstance {
  return hostTool({
    name: "list_sandbox_snapshots",
    description:
      "List filesystem snapshots for the current Exo sandbox. Use scope 'agent' or null for the shared persistent agent sandbox; use 'conversation' only when the conversation has its own sandbox.",
    parameters: scopeParameters(),
  });
}

function snapshotSandboxTool(): ToolInstance {
  return hostTool({
    name: "snapshot_sandbox",
    description:
      "Capture a filesystem snapshot of the current Exo sandbox so it can be rewound later. Use this before risky edits or experiments.",
    parameters: scopeParameters(),
  });
}

function rewindSandboxTool(): ToolInstance {
  return hostTool({
    name: "rewind_sandbox",
    description:
      "Rewind the current Exo sandbox to a snapshot returned by list_sandbox_snapshots or snapshot_sandbox. This replaces the live sandbox filesystem state for the selected scope.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        scope: scopeProperty(),
        snapshotId: {
          type: "string",
          description:
            "Snapshot id returned by snapshot_sandbox or list_sandbox_snapshots.",
        },
      },
      required: ["scope", "snapshotId"],
    },
  });
}

function scopeParameters(): ToolDefinition["parameters"] {
  return {
    type: "object",
    additionalProperties: false,
    properties: {
      scope: scopeProperty(),
    },
    required: ["scope"],
  };
}

function scopeProperty(): ToolDefinition["parameters"] {
  return {
    type: ["string", "null"],
    enum: ["agent", "conversation", null],
    description:
      "Sandbox scope. Use 'agent' or null for Exo's shared persistent agent sandbox; use 'conversation' for this conversation's sandbox.",
  };
}
