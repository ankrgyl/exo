import type { HarnessToolRegistry } from "@exo/harness";

import { registerHostTool } from "./host-tools";

// Read-only introspection over the agent's own history: adapter telemetry
// from the AdapterStore and the canonical exoharness conversation event log
// (which host components also write to, e.g. host_reboot when the service
// guardian restarts the adapter runner). These let Exo diagnose a quiet
// or failing adapter, and reconstruct what happened to it (reboots, drains,
// errors) without parsing .exo files.
export function registerIntrospectionTools(
  registry: HarnessToolRegistry,
): void {
  registerHostTool(registry, {
    name: "list_adapter_events",
    description:
      "List recent telemetry events for one adapter in this conversation, newest first: connected, disconnected, inbound, outbound, error, and lifecycle records. Use this to diagnose an adapter that seems quiet or unhealthy, after checking last_connected_at_ms and last_error from list_adapters. Read-only.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        adapterId: {
          type: "string",
          description: "Adapter id from list_adapters.",
        },
        eventType: {
          type: ["string", "null"],
          enum: [
            "connected",
            "disconnected",
            "inbound",
            "outbound",
            "error",
            "lifecycle",
            null,
          ],
          description: "Only return events of this type. Null for all types.",
        },
        sinceMs: {
          type: ["number", "null"],
          description:
            "Only return events created at or after this unix epoch milliseconds timestamp. Null for no lower bound.",
        },
        limit: {
          type: ["number", "null"],
          description:
            "Maximum events to return (default 50, capped at 200). Null for the default.",
        },
      },
      required: ["adapterId", "eventType", "sinceMs", "limit"],
    },
  });
  registerHostTool(registry, {
    name: "list_conversation_events",
    description:
      'List this conversation\'s canonical event log, newest first. By default returns lifecycle and host events only: conversation_created, conversation_forked, session_started, session_ended, error, sandbox_created, sandbox_started, sandbox_stopped, sandbox_snapshotted, host_reboot (planned host restart with reason), adapter_runner_started (a start without a preceding host_reboot implies a crash or manual restart), and adapter_runner_draining (graceful shutdown began), exo.compaction.v1 (older history was replaced by a summary) and exo.compaction.failed.v1. Pass explicit kinds to query other event types (e.g. tool_requested, tool_result, messages — these can be very large). Each messages event carries a usage annotation with token counts and cost_usd for that model turn, so this tool is also how to answer questions about spend: query kinds ["messages"] and sum usage.cost_usd. Use this to reconstruct restarts, crashes, session history, and per-conversation cost. Read-only.',
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        kinds: {
          type: ["array", "null"],
          items: { type: "string" },
          description:
            "Event kinds to return. Null for the default lifecycle/host set described above.",
        },
        limit: {
          type: ["number", "null"],
          description:
            "Maximum events to return (default 50, capped at 200). Null for the default.",
        },
        cursor: {
          type: ["string", "null"],
          description:
            "Event id cursor from a previous call's result for pagination. Null to start from the newest (or oldest for asc) event.",
        },
        direction: {
          type: ["string", "null"],
          enum: ["asc", "desc", null],
          description: "Listing order. Null for desc (newest first).",
        },
      },
      required: ["kinds", "limit", "cursor", "direction"],
    },
  });
}
