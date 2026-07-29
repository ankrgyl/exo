import { describe, expect, it } from "vitest";

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";

import type {
  EventData,
  JsonObject,
  PendingToolCall,
  ToolInstance,
  ToolResult,
} from "@exo/harness";

import {
  callRegistryTool,
  injectedToolDefinitions,
  registryMcpServer,
  toolResultOk,
  type ToolRegistryHandle,
} from "./registry-tools";

const GREET_SCHEMA = {
  type: "object",
  additionalProperties: false,
  properties: { name: { type: "string", description: "Who to greet." } },
  required: ["name"],
};

// In-memory registry fake; records calls so tests can assert on dispatch.
class FakeRegistry implements ToolRegistryHandle {
  readonly calls: PendingToolCall[] = [];

  constructor(
    private readonly tools: Record<string, (args: JsonObject) => ToolResult>,
  ) {}

  definitions() {
    return Object.keys(this.tools).map((name) => ({
      name,
      description: `${name} tool`,
      parameters: GREET_SCHEMA,
    }));
  }

  get(name: string): ToolInstance | undefined {
    const run = this.tools[name];
    if (!run) {
      return undefined;
    }
    return {
      source: "library",
      definition: {
        name,
        description: `${name} tool`,
        parameters: GREET_SCHEMA,
      },
      handler: { execute: async (args) => run(args) },
    };
  }

  async executePending(toolCalls: PendingToolCall[]): Promise<EventData[]> {
    const events: EventData[] = [];
    for (const toolCall of toolCalls) {
      this.calls.push(toolCall);
      const run = this.tools[toolCall.request.functionName];
      const result = run
        ? run(toolCall.request.arguments)
        : { ok: false, error: "not registered" };
      events.push({
        type: "tool_result",
        tool_call_id: toolCall.toolCallId,
        result,
      });
    }
    return events;
  }
}

function pendingCall(name: string, args: JsonObject = {}): PendingToolCall {
  return {
    toolCallId: "call-1",
    request: { functionName: name, arguments: args },
  };
}

describe("injectedToolDefinitions", () => {
  it("passes the JSON schema through verbatim", () => {
    const registry = new FakeRegistry({ greet: () => ({ ok: true }) });
    expect(injectedToolDefinitions(registry)).toEqual([
      { name: "greet", description: "greet tool", inputSchema: GREET_SCHEMA },
    ]);
  });
});

describe("toolResultOk", () => {
  it("treats ok: false as failure and everything else as success", () => {
    expect(toolResultOk({ ok: false, error: "nope" })).toBe(false);
    expect(toolResultOk({ ok: true })).toBe(true);
    expect(toolResultOk({ files_changed: 2 })).toBe(true);
    expect(toolResultOk("plain text")).toBe(true);
    expect(toolResultOk(null)).toBe(true);
  });
});

describe("callRegistryTool", () => {
  it("executes the tool and returns its result with events", async () => {
    const registry = new FakeRegistry({
      greet: (args) => ({ ok: true, greeting: `hi ${String(args.name)}` }),
    });
    const outcome = await callRegistryTool(
      registry,
      pendingCall("greet", { name: "exo" }),
    );
    expect(outcome.result).toEqual({ ok: true, greeting: "hi exo" });
    expect(outcome.ok).toBe(true);
    expect(outcome.events).toHaveLength(1);
    expect(registry.calls).toHaveLength(1);
  });

  it("reports a failing tool as not ok", async () => {
    const registry = new FakeRegistry({
      greet: () => ({ ok: false, error: "bad mood" }),
    });
    const outcome = await callRegistryTool(registry, pendingCall("greet"));
    expect(outcome.ok).toBe(false);
  });

  it("rejects an unknown tool without executing anything", async () => {
    const registry = new FakeRegistry({ greet: () => ({ ok: true }) });
    const outcome = await callRegistryTool(registry, pendingCall("missing"));
    expect(outcome.ok).toBe(false);
    expect(JSON.stringify(outcome.result)).toContain("missing");
    expect(registry.calls).toHaveLength(0);
    expect(outcome.events).toHaveLength(0);
  });
});

describe("registryMcpServer", () => {
  async function connect(registry: ToolRegistryHandle): Promise<Client> {
    const server = registryMcpServer(registry);
    const [clientTransport, serverTransport] =
      InMemoryTransport.createLinkedPair();
    const client = new Client({ name: "test", version: "0.0.0" });
    await Promise.all([
      server.connect(serverTransport),
      client.connect(clientTransport),
    ]);
    return client;
  }

  it("lists registry tools with their schemas", async () => {
    const client = await connect(
      new FakeRegistry({ greet: () => ({ ok: true }) }),
    );
    const listed = await client.listTools();
    expect(listed.tools).toHaveLength(1);
    expect(listed.tools[0].name).toBe("greet");
    expect(listed.tools[0].inputSchema).toEqual(GREET_SCHEMA);
  });

  it("routes a tool call to the registry and returns the result", async () => {
    const registry = new FakeRegistry({
      greet: (args) => ({ ok: true, greeting: `hi ${String(args.name)}` }),
    });
    const client = await connect(registry);
    const result = await client.callTool({
      name: "greet",
      arguments: { name: "exo" },
    });
    expect(result.isError).toBeFalsy();
    expect(result.content).toEqual([
      { type: "text", text: JSON.stringify({ ok: true, greeting: "hi exo" }) },
    ]);
  });

  it("marks a failing tool call as an error", async () => {
    const client = await connect(
      new FakeRegistry({ greet: () => ({ ok: false, error: "bad mood" }) }),
    );
    const result = await client.callTool({ name: "greet", arguments: {} });
    expect(result.isError).toBe(true);
  });

  it("marks an unknown tool as an error instead of throwing", async () => {
    const client = await connect(new FakeRegistry({}));
    const result = await client.callTool({ name: "nope", arguments: {} });
    expect(result.isError).toBe(true);
  });
});
