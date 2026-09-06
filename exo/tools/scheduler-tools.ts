import type {
  HarnessToolRegistry,
  ToolDefinition,
  ToolInstance,
} from "@exo/harness";

export type SchedulerToolName =
  | "schedule_sandbox_task"
  | "list_scheduled_tasks"
  | "cancel_scheduled_task"
  | "delete_scheduled_task";

export function createSchedulerToolInstances(): ToolInstance[] {
  return [
    createScheduleSandboxTaskTool(),
    createListScheduledTasksTool(),
    createCancelScheduledTaskTool(),
    createDeleteScheduledTaskTool(),
  ];
}

export function registerSchedulerTools(
  registry: HarnessToolRegistry,
  names: SchedulerToolName[] = [
    "schedule_sandbox_task",
    "list_scheduled_tasks",
    "cancel_scheduled_task",
    "delete_scheduled_task",
  ],
): void {
  const requested = new Set<SchedulerToolName>(names);
  for (const tool of createSchedulerToolInstances()) {
    if (requested.has(tool.definition.name as SchedulerToolName)) {
      registry.register(tool);
    }
  }
}

function createScheduleSandboxTaskTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "schedule_sandbox_task",
      description:
        "Schedule a recurring command to run in this Exo agent's shared sandbox by default. A host scheduler owns timing and will wake this conversation with compact results when runs complete. Use setupCommand for task-specific setup that should run before each scheduled run.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description:
              "Stable task name using letters, numbers, dashes, or underscores.",
          },
          schedule: {
            type: "string",
            description:
              "Schedule as '@every 10m', '@every 1h', a simple cron interval like '*/30 * * * *', wall-clock cron like '0 0 5 * * *' (5 fields) or '*/10 * * * * *' (6 fields; sec min hour day month weekday, UTC), or '@at 2026-07-26T17:00:00Z' for a one-shot. Recurring fires land on the schedule's own grid rather than on when the last run finished, so a slow run does not push later fires later. A one-shot fires once, is then marked completed, and stays listed without ever running again.",
          },
          missed: {
            anyOf: [
              { type: "string", enum: ["skip", "once", "all"] },
              { type: "null" },
            ],
            description:
              "What a host that was down owes this task once it comes back. Use 'skip' to drop every missed slot and resume at the next future one, 'once' or null to fire a single catch-up run and then resume on the grid, or 'all' to fire each missed slot in order, capped at 100.",
          },
          command: {
            type: "array",
            items: { type: "string" },
            minItems: 1,
            description:
              "Command argv to run in the sandbox, for example ['bash', '-lc', 'curl -fsSL https://example.com/health'].",
          },
          sandboxMode: {
            anyOf: [
              { type: "string", enum: ["agent", "conversation", "task_fresh"] },
              { type: "null" },
            ],
            description:
              "Sandbox selection mode. Use 'agent' or null to run in the shared persistent agent sandbox. Use 'conversation' for a sandbox scoped to this conversation. Use 'task_fresh' to create a separate fresh sandbox for this task and reuse it across that task's runs.",
          },
          setupCommand: {
            anyOf: [
              {
                type: "array",
                items: { type: "string" },
                minItems: 1,
              },
              { type: "null" },
            ],
            description:
              "Optional argv to run immediately before each scheduled run, for example ['bash', '-lc', 'apt-get update && apt-get install -y curl']. Use this for dependencies that should be prepared before each run.",
          },
          reportPrompt: {
            type: "string",
            description:
              "Instructions for how to report each completed run back to the user.",
          },
          maxOutputBytes: {
            type: ["number", "null"],
            description:
              "Maximum bytes to retain from each output stream before truncating, or null for the default.",
          },
        },
        required: [
          "name",
          "schedule",
          "missed",
          "command",
          "sandboxMode",
          "setupCommand",
          "reportPrompt",
          "maxOutputBytes",
        ],
      },
    },
    handler: {
      execute(args, execution) {
        return execution.context.executeTool({
          functionName: "schedule_sandbox_task",
          arguments: args,
        });
      },
    },
  };
}

function createListScheduledTasksTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "list_scheduled_tasks",
      description:
        "List scheduled sandbox tasks for this conversation. Disabled tasks are hidden unless includeDisabled is true. Each task carries its missed-fire policy as 'missed', the last missed-fire evaluation as 'last_missed_fire', and 'completed_at_ms' once a one-shot has fired.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          includeDisabled: {
            type: ["boolean", "null"],
            description:
              "Whether to include disabled/cancelled tasks. Use false or null for the default active-task view.",
          },
        },
        required: ["includeDisabled"],
      },
    },
    handler: {
      execute(args, execution) {
        return execution.context.executeTool({
          functionName: "list_scheduled_tasks",
          arguments: args,
        });
      },
    },
  };
}

function createDeleteScheduledTaskTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "delete_scheduled_task",
      description:
        "Permanently delete a scheduled sandbox task for this conversation, including its stored run history. Use cancel_scheduled_task instead when history should be preserved.",
      parameters: taskIdParameters(
        "Scheduled task id returned by schedule_sandbox_task or list_scheduled_tasks.",
      ),
    },
    handler: {
      execute(args, execution) {
        return execution.context.executeTool({
          functionName: "delete_scheduled_task",
          arguments: args,
        });
      },
    },
  };
}

function createCancelScheduledTaskTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "cancel_scheduled_task",
      description: "Disable a scheduled sandbox task for this conversation.",
      parameters: taskIdParameters(
        "Scheduled task id returned by schedule_sandbox_task or list_scheduled_tasks.",
      ),
    },
    handler: {
      execute(args, execution) {
        return execution.context.executeTool({
          functionName: "cancel_scheduled_task",
          arguments: args,
        });
      },
    },
  };
}

function taskIdParameters(description: string): ToolDefinition["parameters"] {
  return {
    type: "object",
    additionalProperties: false,
    properties: {
      taskId: {
        type: "string",
        description,
      },
    },
    required: ["taskId"],
  };
}
