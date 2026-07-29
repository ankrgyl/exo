import { randomUUID } from "node:crypto";

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";

import type {
  EventData,
  HarnessToolRegistry,
  JsonObject,
  PendingToolCall,
  ToolResult,
} from "@exo/harness";

// Exposes harness registry tools inside the native coding-agent runtimes:
// codex takes them as dynamic tools, Claude Code as an in-process MCP server.
// Built-ins are not injected; the native runtimes bring their own shell and
// file tools. Execution stays on the host, where the registry lives.

// The registry subset the bridge needs, so tests can fake it.
export type ToolRegistryHandle = Pick<
  HarnessToolRegistry,
  "definitions" | "get" | "executePending"
>;

export interface InjectedToolDefinition {
  name: string;
  description: string;
  inputSchema: JsonObject;
}

export function injectedToolDefinitions(
  registry: ToolRegistryHandle,
): InjectedToolDefinition[] {
  return registry.definitions().map((definition) => ({
    name: definition.name,
    description: definition.description,
    // ToolDefinition.parameters is already a JSON schema object; both
    // runtimes take it verbatim, so nothing is lost in translation.
    inputSchema: definition.parameters as JsonObject,
  }));
}

export interface RegistryToolOutcome {
  result: ToolResult;
  ok: boolean;
  events: EventData[];
}

// Tools report failure as { ok: false }; anything else counts as success.
export function toolResultOk(result: ToolResult): boolean {
  return !(
    typeof result === "object" &&
    result !== null &&
    !Array.isArray(result) &&
    result.ok === false
  );
}

export async function callRegistryTool(
  registry: ToolRegistryHandle,
  toolCall: PendingToolCall,
): Promise<RegistryToolOutcome> {
  if (!registry.get(toolCall.request.functionName)) {
    const result: ToolResult = {
      ok: false,
      error: `unknown tool: ${toolCall.request.functionName}`,
    };
    return { result, ok: false, events: [] };
  }
  const events = await registry.executePending([toolCall]);
  const resultEvent = events.find((event) => event.type === "tool_result");
  const result = (resultEvent?.result ?? {
    ok: false,
    error: "tool produced no result",
  }) as ToolResult;
  return { result, ok: toolResultOk(result), events };
}

// The instance goes into claude-agent-sdk options as
// { type: "sdk", name, instance }; the SDK runs it in this process, so calls
// land on the registry without leaving the host. Raw request handlers rather
// than registerTool, which would force the JSON schemas through zod.
export function registryMcpServer(registry: ToolRegistryHandle): McpServer {
  const server = new McpServer(
    { name: "exo", version: "0.0.0" },
    { capabilities: { tools: {} } },
  );
  server.server.setRequestHandler(ListToolsRequestSchema, () => ({
    tools: injectedToolDefinitions(registry).map((definition) => ({
      ...definition,
      inputSchema: definition.inputSchema as { [key: string]: unknown } & {
        type: "object";
      },
    })),
  }));
  server.server.setRequestHandler(CallToolRequestSchema, async (request) => {
    const toolCall: PendingToolCall = {
      toolCallId: `injected-${randomUUID()}`,
      request: {
        functionName: request.params.name,
        arguments: (request.params.arguments ?? {}) as JsonObject,
      },
    };
    // No events appended here: the Claude message observer already records
    // MCP tool calls and results from the SDK message stream.
    const { result, ok } = await callRegistryTool(registry, toolCall);
    return {
      content: [{ type: "text" as const, text: JSON.stringify(result) }],
      isError: !ok,
    };
  });
  return server;
}
