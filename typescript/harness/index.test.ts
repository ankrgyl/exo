import { describe, expect, it } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";

import {
  buildShellToolDefinitions,
  createShellToolInstance,
  createToolRegistry,
  initializeTool,
  registerBuiltInTools,
  registerAgentTools,
  registerAgentToolsFromDirectoryIfExists,
  registerLibraryTools,
  registerLibraryToolModulePath,
  registerTools,
  materializeEventsToMessages,
  toolResultMessage,
  toolResultEvent,
  type Event,
  type EventData,
  type JsonObject,
  type ToolExecutionContext,
  type ToolInstance,
  type Tool,
  type ToolResult,
  type TurnContext,
} from "./index";
import { ircTool } from "../../examples/typescript/tools/irc";
import { uppercaseTool } from "../../examples/typescript/tools/uppercase";

describe("HarnessToolRegistry", () => {
  it("returns registered tool definitions", () => {
    const context = fakeTurnContext();
    const tool = fakeTool("echo", async (args) => args);
    const registry = createToolRegistry(context).register(tool);

    expect(registry.definitions()).toEqual([tool.definition]);
    expect(registry.get("echo")).toBe(tool);
  });

  it("rejects duplicate tool names", () => {
    const context = fakeTurnContext();
    const registry = createToolRegistry(context).register(
      fakeTool("echo", async (args) => args),
    );

    expect(() =>
      registry.register(fakeTool("echo", async (args) => args)),
    ).toThrow("tool is already registered: echo");
  });

  it("executes pending tool calls and returns tool result events", async () => {
    const context = fakeTurnContext();
    const executionContexts: ToolExecutionContext[] = [];
    const registry = createToolRegistry(context).register(
      fakeTool("echo", async (args, execution) => {
        executionContexts.push(execution);
        return { echoed: args.value };
      }),
    );

    const events = await registry.executePending([
      {
        toolCallId: "call_1",
        request: {
          functionName: "echo",
          arguments: { value: "hello" },
        },
      },
    ]);

    expect(events).toEqual([
      wrappedToolResultEvent("call_1", "echo", "library", 1, {
        echoed: "hello",
      }),
    ]);
    expect(executionContexts).toHaveLength(1);
    expect(executionContexts[0].context).toBe(context);
    expect(executionContexts[0].toolCallId).toBe("call_1");
  });

  it("emits stream events around tool execution when streaming", async () => {
    const streamEvents: EventData[] = [];
    const context = fakeTurnContext({
      streaming: true,
      streamEvents,
    });
    const registry = createToolRegistry(context).register(
      fakeTool("echo", async (args) => ({ echoed: args.value })),
    );

    await registry.executePending([
      {
        toolCallId: "call_1",
        request: {
          functionName: "echo",
          arguments: { value: "hello" },
        },
      },
    ]);

    expect(streamEvents).toEqual([
      {
        type: "tool_call_streamed",
        toolCallId: "call_1",
        toolName: "echo",
        arguments: { value: "hello" },
      },
      {
        type: "tool_result_streamed",
        toolCallId: "call_1",
        result: wrappedToolResult("call_1", "echo", "library", 1, {
          echoed: "hello",
        }),
      },
    ]);
  });

  it("throws for unregistered tools", async () => {
    const registry = createToolRegistry(fakeTurnContext());

    await expect(
      registry.executePending([
        {
          toolCallId: "call_1",
          request: {
            functionName: "missing",
            arguments: {},
          },
        },
      ]),
    ).resolves.toEqual([
      wrappedToolResultEvent("call_1", "missing", "built_in", 1, {
        ok: false,
        error: "tool execution is not configured for missing",
      }),
    ]);
  });

  it("returns tool result errors instead of throwing tool failures", async () => {
    const context = fakeTurnContext();
    const registry = createToolRegistry(context).register(
      fakeTool("fail", async () => {
        throw new Error("boom");
      }),
    );

    await expect(
      registry.executePending([
        {
          toolCallId: "call_1",
          request: {
            functionName: "fail",
            arguments: {},
          },
        },
      ]),
    ).resolves.toEqual([
      wrappedToolResultEvent("call_1", "fail", "library", 1, {
        ok: false,
        error: "boom",
      }),
    ]);
  });

  it("stores large shell-style output in artifacts instead of inline value", async () => {
    const context = fakeTurnContext();
    const stdout = "x".repeat(9_000);
    const registry = createToolRegistry(context).register(
      fakeTool("shell", async () => ({
        stdout,
        stderr: "",
        exit_code: 0,
      })),
    );

    const events = await registry.executePending([
      {
        toolCallId: "call_1",
        request: {
          functionName: "shell",
          arguments: {},
        },
      },
    ]);

    expect(events).toEqual([
      toolResultEvent("call_1", {
        ok: true,
        toolName: "shell",
        toolCallId: "call_1",
        source: "library",
        resultArtifact: {
          artifactId: "artifact-1",
          path: "tool-results/shell/call_1/result.json",
          version: 1,
          sizeBytes: 9053,
          mimeType: "application/json",
        },
        artifacts: [
          {
            artifactId: "artifact-1",
            path: "tool-results/shell/call_1/result.json",
            version: 1,
            sizeBytes: 9053,
            mimeType: "application/json",
          },
          {
            artifactId: "artifact-2",
            path: "tool-results/shell/call_1/stdout.txt",
            version: 1,
            sizeBytes: 9000,
            mimeType: "text/plain",
          },
        ],
        truncated: true,
        preview: `${JSON.stringify(
          {
            stdout,
            stderr: "",
            exit_code: 0,
          },
          null,
          2,
        ).slice(0, 4_000)}\n...[truncated]`,
        value: null,
      }),
    ]);
  });
});

describe("materializeEventsToMessages", () => {
  it("synthesizes results for dangling tool calls before later messages", () => {
    const events: Event[] = [
      {
        id: "1",
        conversationId: "conversation",
        createdAt: "2026-01-01T00:00:00Z",
        data: {
          type: "messages",
          messages: [
            {
              role: "assistant",
              content: [
                {
                  type: "tool_call",
                  tool_call_id: "call_1",
                  tool_name: "install_agent_tool",
                  arguments: {},
                },
              ],
            },
          ],
        },
      },
      {
        id: "2",
        conversationId: "conversation",
        createdAt: "2026-01-01T00:00:01Z",
        data: {
          type: "tool_requested",
          tool_call_id: "call_1",
          request: {
            function_name: "install_agent_tool",
            arguments: {},
          },
        },
      },
      {
        id: "3",
        conversationId: "conversation",
        createdAt: "2026-01-01T00:00:02Z",
        data: {
          type: "messages",
          messages: [{ role: "user", content: "try again" }],
        },
      },
    ];

    expect(materializeEventsToMessages(events)).toEqual([
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_1",
            tool_name: "install_agent_tool",
            arguments: {},
          },
        ],
      },
      toolResultMessage("call_1", "install_agent_tool", {
        ok: false,
        error: "tool execution did not complete before the previous turn ended",
      }),
      { role: "user", content: "try again" },
    ]);
  });
});

describe("shell built-in tool", () => {
  it("builds the existing shell tool definition shape", () => {
    expect(
      buildShellToolDefinitions({
        shellProgram: "/bin/bash",
        mounts: [],
      }),
    ).toEqual([
      {
        name: "shell",
        description: "Run a shell command using /bin/bash.",
        parameters: {
          type: "object",
          additionalProperties: false,
          properties: {
            command: {
              type: "string",
              description: "Shell command to execute.",
            },
          },
          required: ["command"],
        },
      },
    ]);
  });

  it("omits the shell definition when shell is disabled", () => {
    expect(
      buildShellToolDefinitions({
        shellProgram: null,
        mounts: [],
      }),
    ).toEqual([]);
  });

  it("delegates shell execution to the host tool path", async () => {
    const executedRequests: JsonObject[] = [];
    const context = fakeTurnContext({
      executeTool: async (request) => {
        executedRequests.push({
          functionName: request.functionName,
          arguments: request.arguments,
        });
        return {
          stdout: "ok\n",
          stderr: "",
          exit_code: 0,
        };
      },
    });
    const shell = createShellToolInstance({
      shellProgram: "/bin/bash",
      mounts: [],
    });

    expect(shell).not.toBeNull();
    const result = await shell!.handler.execute(
      { command: "echo ok" },
      {
        context,
        toolCallId: "call_1",
      },
    );

    expect(executedRequests).toEqual([
      {
        functionName: "shell",
        arguments: { command: "echo ok" },
      },
    ]);
    expect(result).toEqual({
      stdout: "ok\n",
      stderr: "",
      exit_code: 0,
    });
  });

  it("registers requested built-in tools", () => {
    const context = fakeTurnContext({
      conversationConfig: {
        shellProgram: "/bin/bash",
        mounts: [],
      },
    });
    const registry = createToolRegistry(context);

    registerBuiltInTools(registry, context, ["shell"]);

    expect(registry.definitions()).toEqual(
      buildShellToolDefinitions(context.conversationConfig),
    );
  });
});

describe("library tool modules", () => {
  it("initializes, registers, and executes a direct TypeScript tool", async () => {
    const context = fakeTurnContext();
    const tool = await initializeTool(
      uppercaseTool,
      "library",
      {
        prefix: "result: ",
      },
      context,
    );
    const registry = createToolRegistry(context).register(tool);

    expect(registry.definitions()).toEqual([uppercaseTool.definition]);
    await expect(
      registry.executePending([
        {
          toolCallId: "call_1",
          request: {
            functionName: "uppercase",
            arguments: {
              text: "hello",
            },
          },
        },
      ]),
    ).resolves.toEqual([
      wrappedToolResultEvent("call_1", "uppercase", "library", 1, {
        text: "result: HELLO",
      }),
    ]);
  });

  it("initializes and executes the demo IRC tool in dry-run mode", async () => {
    const context = fakeTurnContext();
    const tool = await initializeTool(
      ircTool,
      "library",
      {
        server: "irc.example.test",
        port: 6697,
        nick: "exo-agent",
        username: "exo",
        realname: "Exo Agent",
        tls: true,
        dryRun: true,
        passwordSecretId: null,
      },
      context,
    );
    const registry = createToolRegistry(context).register(tool);

    expect(registry.definitions()).toEqual([ircTool.definition]);
    await expect(
      registry.executePending([
        {
          toolCallId: "call_1",
          request: {
            functionName: "irc_send_message",
            arguments: {
              channel: "#exo",
              text: "hello",
            },
          },
        },
      ]),
    ).resolves.toEqual([
      wrappedToolResultEvent("call_1", "irc_send_message", "library", 1, {
        ok: true,
        dryRun: true,
        registered: false,
        joined: false,
        server: "irc.example.test",
        channel: "#exo",
      }),
    ]);
  });
});

describe("agent tool loading", () => {
  it("loads and registers library tools from exported module data", async () => {
    const context = fakeTurnContext();
    const registry = createToolRegistry(context);

    await registerLibraryTools(registry, context, {
      tool: uppercaseTool,
      initialization: {
        prefix: "library: ",
      },
    });

    expect(registry.get("uppercase")?.source).toBe("library");
    await expect(
      registry.executePending([
        {
          toolCallId: "call_1",
          request: {
            functionName: "uppercase",
            arguments: {
              text: "hello",
            },
          },
        },
      ]),
    ).resolves.toEqual([
      wrappedToolResultEvent("call_1", "uppercase", "library", 1, {
        text: "library: HELLO",
      }),
    ]);
  });

  it("loads and registers agent tools from exported module data", async () => {
    const context = fakeTurnContext();
    const registry = createToolRegistry(context);

    await registerAgentTools(registry, context, {
      tool: uppercaseTool,
      initialization: {
        prefix: "agent: ",
      },
    });

    expect(registry.definitions()).toEqual([uppercaseTool.definition]);
    expect(registry.get("uppercase")?.source).toBe("agent");
    await expect(
      registry.executePending([
        {
          toolCallId: "call_1",
          request: {
            functionName: "uppercase",
            arguments: {
              text: "hello",
            },
          },
        },
      ]),
    ).resolves.toEqual([
      wrappedToolResultEvent("call_1", "uppercase", "agent", 1, {
        text: "agent: HELLO",
      }),
    ]);
  });

  it("loads tools through the generic source-aware module path", async () => {
    const context = fakeTurnContext();
    const registry = createToolRegistry(context);

    await registerTools(
      registry,
      context,
      { tool: uppercaseTool, initialization: { prefix: "generic: " } },
      "library",
    );

    expect(registry.get("uppercase")?.source).toBe("library");
  });

  it("loads library tool configuration from a TypeScript module export", async () => {
    const context = fakeTurnContext();
    const registry = createToolRegistry(context);

    await registerLibraryToolModulePath(registry, context, ircToolModulePath());

    expect(registry.get("irc_send_message")?.source).toBe("library");
  });

  it("installs an agent tool and loads it from the default tools directory", async () => {
    const previousCwd = process.cwd();
    const tempdir = await fs.mkdtemp(path.join(os.tmpdir(), "exo-agent-tool-"));
    process.chdir(tempdir);
    try {
      const context = fakeTurnContext();
      const installerRegistry = createToolRegistry(context);
      registerBuiltInTools(installerRegistry, context, ["install_agent_tool"]);

      await expect(
        installerRegistry.executePending([
          {
            toolCallId: "install_1",
            request: {
              functionName: "install_agent_tool",
              arguments: {
                name: "reverse-text",
                moduleSource: reverseTextToolSource(),
                initialization: {},
              },
            },
          },
        ]),
      ).resolves.toEqual([
        wrappedToolResultEvent(
          "install_1",
          "install_agent_tool",
          "built_in",
          1,
          {
            ok: true,
            toolName: "reverse_text",
            modulePath: ".exo/agent-tools/reverse-text.ts",
            availableNextRound: true,
          },
        ),
      ]);

      const registry = createToolRegistry(context);
      await registerAgentToolsFromDirectoryIfExists(registry, context);

      expect(registry.get("reverse_text")?.source).toBe("agent");
      await expect(
        registry.executePending([
          {
            toolCallId: "call_1",
            request: {
              functionName: "reverse_text",
              arguments: {
                text: "hello",
              },
            },
          },
        ]),
      ).resolves.toEqual([
        wrappedToolResultEvent("call_1", "reverse_text", "agent", 2, {
          text: "olleh",
        }),
      ]);
    } finally {
      process.chdir(previousCwd);
      await fs.rm(tempdir, { recursive: true, force: true });
    }
  });

  it("rejects installing an agent tool whose tool name belongs to another module", async () => {
    const previousCwd = process.cwd();
    const tempdir = await fs.mkdtemp(path.join(os.tmpdir(), "exo-agent-tool-"));
    process.chdir(tempdir);
    try {
      const context = fakeTurnContext();
      const registry = createToolRegistry(context);
      registerBuiltInTools(registry, context, ["install_agent_tool"]);

      // Two different module files whose source defines the same tool name,
      // reverse_text: only the first install may claim that tool name.
      const moduleSource = reverseTextToolSource();
      await registry.executePending([
        installAgentToolCall("install_1", "reverse-text", moduleSource),
      ]);
      const events = await registry.executePending([
        installAgentToolCall("install_2", "reverse-text-v2", moduleSource),
      ]);

      expect(events).toEqual([
        wrappedToolResultEvent(
          "install_2",
          "install_agent_tool",
          "built_in",
          2,
          {
            ok: false,
            error:
              "tool reverse_text is already installed by module reverse-text; " +
              "reinstall using name reverse-text to replace it, or uninstall_agent_tool it first",
          },
        ),
      ]);
      await expect(
        fs.access(".exo/agent-tools/reverse-text-v2.ts"),
      ).rejects.toThrow();
    } finally {
      process.chdir(previousCwd);
      await fs.rm(tempdir, { recursive: true, force: true });
    }
  });

  it("skips broken and duplicate agent tool modules instead of failing startup", async () => {
    const previousCwd = process.cwd();
    const tempdir = await fs.mkdtemp(path.join(os.tmpdir(), "exo-agent-tool-"));
    process.chdir(tempdir);
    try {
      const toolsDirectory = ".exo/agent-tools";
      await fs.mkdir(toolsDirectory, { recursive: true });
      await fs.writeFile(
        path.join(toolsDirectory, "a-broken.ts"),
        "this is not valid typescript {{{\n",
        "utf8",
      );
      await fs.writeFile(
        path.join(toolsDirectory, "b-reverse-text.ts"),
        reverseTextToolSource(),
        "utf8",
      );
      await fs.writeFile(
        path.join(toolsDirectory, "c-duplicate-reverse-text.ts"),
        reverseTextToolSource(),
        "utf8",
      );

      const context = fakeTurnContext();
      const registry = createToolRegistry(context);
      await registerAgentToolsFromDirectoryIfExists(registry, context);

      expect(registry.get("reverse_text")?.source).toBe("agent");
      await expect(
        registry.executePending([
          {
            toolCallId: "call_1",
            request: {
              functionName: "reverse_text",
              arguments: { text: "hello" },
            },
          },
        ]),
      ).resolves.toEqual([
        wrappedToolResultEvent("call_1", "reverse_text", "agent", 1, {
          text: "olleh",
        }),
      ]);
    } finally {
      process.chdir(previousCwd);
      await fs.rm(tempdir, { recursive: true, force: true });
    }
  });

  it("rejects agent tool modules without a default Tool export", async () => {
    const registry = createToolRegistry(fakeTurnContext());

    await expect(
      registerAgentTools(registry, fakeTurnContext(), {
        notATool: true,
      } as never),
    ).rejects.toThrow(
      "agent tool module export must be a Tool, ToolModuleEntry, or ToolModule",
    );
  });

  it("rejects invalid agent tool initialization", async () => {
    const registry = createToolRegistry(fakeTurnContext());

    await expect(
      registerAgentTools(registry, fakeTurnContext(), {
        tool: uppercaseTool,
        initialization: {},
      }),
    ).rejects.toThrow("tool initialization.prefix is required");
  });

  it("rejects generated tools using legacy inputSchema and invoke shapes", async () => {
    const generatedTool = {
      definition: {
        name: "curl-tool",
        description: "Fetch a URL.",
        inputSchema: {
          type: "object",
          additionalProperties: false,
          properties: {
            url: { type: "string" },
          },
          required: ["url"],
        },
      },
      initializationParameters: {
        type: "object",
        additionalProperties: false,
        properties: {},
      },
      initialize() {
        return {
          async *invoke() {
            yield { ok: true };
          },
        };
      },
    } as unknown as Tool;

    await expect(
      initializeTool(generatedTool, "agent", {}, fakeTurnContext()),
    ).rejects.toThrow("tool definition must use parameters, not inputSchema");
  });
});

function fakeTool(
  name: string,
  execute: (
    args: JsonObject,
    execution: ToolExecutionContext,
  ) => Promise<ToolResult>,
): ToolInstance {
  return {
    source: "library",
    definition: {
      name,
      description: `Fake ${name} tool.`,
      parameters: {
        type: "object",
        additionalProperties: true,
      },
    },
    handler: {
      execute,
    },
  };
}

function ircToolModulePath(): string {
  return new URL("../../examples/typescript/tools/irc.ts", import.meta.url)
    .href;
}

function installAgentToolCall(
  toolCallId: string,
  moduleName: string,
  moduleSource: string,
) {
  return {
    toolCallId,
    request: {
      functionName: "install_agent_tool",
      arguments: {
        name: moduleName,
        moduleSource,
        initialization: {},
      },
    },
  };
}

function reverseTextToolSource(): string {
  return `
import type { JsonObject, Tool, ToolResult } from "@exo/harness/tool";

const reverseTextTool = {
  definition: {
    name: "reverse_text",
    description: "Reverse text.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        text: { type: "string" },
      },
      required: ["text"],
    },
  },
  initializationParameters: {
    type: "object",
    additionalProperties: false,
    properties: {},
  },
  initialize() {
    return {
      async execute(args: JsonObject): Promise<ToolResult> {
        const value = args.text;
        if (typeof value !== "string") {
          throw new Error("text must be a string");
        }
        return { text: value.split("").reverse().join("") };
      },
    };
  },
} satisfies Tool;

export default reverseTextTool;
`;
}

function wrappedToolResultEvent(
  toolCallId: string,
  toolName: string,
  source: "built_in" | "library" | "agent",
  artifactIndex: number,
  value: ToolResult,
): EventData {
  return toolResultEvent(
    toolCallId,
    wrappedToolResult(toolCallId, toolName, source, artifactIndex, value),
  );
}

function wrappedToolResult(
  toolCallId: string,
  toolName: string,
  source: "built_in" | "library" | "agent",
  artifactIndex: number,
  value: ToolResult,
): ToolResult {
  const serialized =
    typeof value === "string" ? value : JSON.stringify(value, null, 2);
  const artifact = {
    artifactId: `artifact-${artifactIndex}`,
    path: `tool-results/${toolName}/${toolCallId}/result.json`,
    version: 1,
    sizeBytes: `${serialized}\n`.length,
    mimeType: "application/json",
  };
  return {
    ok: resultOk(value),
    toolName,
    toolCallId,
    source,
    resultArtifact: artifact,
    artifacts: [artifact],
    truncated: false,
    preview: serialized,
    value,
  };
}

function resultOk(value: ToolResult): boolean {
  return (
    !value ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    typeof (value as { ok?: unknown }).ok !== "boolean" ||
    (value as { ok: boolean }).ok
  );
}

function fakeTurnContext(
  options: {
    streaming?: boolean;
    streamEvents?: EventData[];
    executeTool?: TurnContext["executeTool"];
    conversationConfig?: TurnContext["conversationConfig"];
  } = {},
): TurnContext {
  const streamEvents = options.streamEvents ?? [];
  let artifactIndex = 0;
  return {
    agentConfig: {
      instructions: [],
      harness: "typescript",
      typescript: null,
      enableAgentToolCreation: true,
      sandbox: {
        image: null,
        provider: "docker",
        mounts: [],
        enableNetworking: false,
        scope: "conversation",
      },
      model: "test-model",
      maxOutputTokens: null,
      maxToolRoundTrips: null,
      braintrust: null,
    },
    conversationConfig: options.conversationConfig ?? {
      shellProgram: null,
      mounts: [],
    },
    request: {
      input: [],
      sessionId: null,
    },
    streaming: options.streaming ?? false,
    braintrustParent: null,
    exoharness: {
      current: {
        agent: {},
        conversation: {
          async writeArtifactText(args: { path: string; text: string }) {
            artifactIndex += 1;
            return {
              artifactId: `artifact-${artifactIndex}`,
              path: args.path,
              version: 1,
              createdAt: "2026-01-01T00:00:00Z",
              sizeBytes: args.text.length,
            };
          },
        },
        turn: {
          async writeArtifactText(args: { path: string; text: string }) {
            artifactIndex += 1;
            return {
              artifactId: `artifact-${artifactIndex}`,
              path: args.path,
              version: 1,
              createdAt: "2026-01-01T00:00:00Z",
              sizeBytes: args.text.length,
            };
          },
        },
      },
    },
    executeTool: options.executeTool ?? (async () => null),
    async startSandboxProcess() {
      throw new Error("not implemented");
    },
    async executePendingTools() {
      return [];
    },
    stream: {
      async firstChunk(ttftMs: number) {
        streamEvents.push({ type: "first_chunk_streamed", ttftMs });
      },
      async text(text: string) {
        streamEvents.push({ type: "text_streamed", text });
      },
      async toolCall(args: {
        toolCallId: string;
        toolName: string;
        arguments: JsonObject;
      }) {
        streamEvents.push({
          type: "tool_call_streamed",
          ...args,
        });
      },
      async toolResult(args: { toolCallId: string; result: ToolResult }) {
        streamEvents.push({
          type: "tool_result_streamed",
          ...args,
        });
      },
    },
  } as unknown as TurnContext;
}
