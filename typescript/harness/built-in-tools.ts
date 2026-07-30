import fs from "node:fs/promises";
import path from "node:path";

import type {
  ConversationConfig,
  JsonObject,
  ToolDefinition,
  ToolResult,
  TurnContext,
} from "./index";
import type { HarnessToolRegistry, ToolInstance } from "./tools";
import {
  DEFAULT_AGENT_TOOL_DIRECTORY,
  findAgentToolNameConflict,
  installToolSource,
  installedToolModulePath,
  loadAgentTool,
  parseInitialization,
  parseToolSource,
  readToolRegistry,
  removeInstalledTool,
  type InstalledTool,
  type ToolSource,
} from "./tool-modules";

export type BuiltInToolName =
  | "shell"
  | "inspect_tools"
  | "manage_tool"
  | "install_agent_tool"
  | "uninstall_agent_tool";

export function registerBuiltInTools(
  registry: HarnessToolRegistry,
  context: TurnContext,
  names: BuiltInToolName[],
): void {
  for (const name of names) {
    if (name === "shell") {
      const shell = createShellToolInstance(context.conversationConfig);
      if (shell) {
        registry.register(shell);
      }
    } else if (name === "inspect_tools") {
      registry.register(createInspectToolsInstance(registry, context));
    } else if (name === "manage_tool") {
      registry.register(createManageToolInstance());
    } else if (name === "install_agent_tool") {
      registry.register(createInstallAgentToolInstance());
    } else if (name === "uninstall_agent_tool") {
      registry.register(createUninstallAgentToolInstance());
    }
  }
}

export function createShellToolInstance(
  config: ConversationConfig,
): ToolInstance | null {
  if (!config.shellProgram) {
    return null;
  }
  return {
    source: "built_in",
    definition: shellToolDefinition(config.shellProgram),
    handler: {
      execute(args, execution) {
        return execution.context.executeTool({
          functionName: "shell",
          arguments: args,
        });
      },
    },
  };
}

export function buildShellToolDefinitions(
  config: ConversationConfig,
): ToolDefinition[] {
  const tool = createShellToolInstance(config);
  return tool ? [tool.definition] : [];
}

function shellToolDefinition(shellProgram: string): ToolDefinition {
  return {
    name: "shell",
    description: `Run a shell command using ${shellProgram}.`,
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
  };
}

export function shellToolRequest(args: JsonObject): {
  functionName: "shell";
  arguments: JsonObject;
} {
  return {
    functionName: "shell",
    arguments: args,
  };
}

export type ShellToolResult = ToolResult;

export function createInspectToolsInstance(
  registry: HarnessToolRegistry,
  context: TurnContext,
): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "inspect_tools",
      description:
        "Inspect tools that are active in this round or installed in the host-local tool registry.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          source: { type: "string", enum: ["active", "installed"] },
          operation: { type: "string", enum: ["list", "get"] },
          toolId: { type: ["string", "null"] },
        },
        required: ["source", "operation", "toolId"],
      },
    },
    handler: {
      execute(args) {
        return inspectTools(registry, context, args);
      },
    },
  };
}

export function createManageToolInstance(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "manage_tool",
      description:
        "Install or remove a manifest-based TypeScript tool from a workspace-relative directory or pinned Git commit. For agent-authored tools, write under /workspace/exo/.exo/tool-sources/<name> with shell and pass .exo/tool-sources/<name> as the local path. The tool's parameters schema must satisfy strict mode: every key in properties also listed in required, with optional parameters typed as nullable. Installing the same stable manifest id replaces the existing installation.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          action: {
            type: "string",
            enum: ["install", "remove"],
          },
          toolId: { type: ["string", "null"] },
          source: {
            anyOf: [
              {
                type: "object",
                additionalProperties: false,
                properties: {
                  type: { type: "string", enum: ["local"] },
                  path: {
                    type: "string",
                    description:
                      "Path relative to the host workspace root. If shell writes under /workspace/exo/.exo/tool-sources/<name>, pass .exo/tool-sources/<name>; never pass /tmp or an absolute sandbox path.",
                  },
                  subdirectory: { type: ["string", "null"] },
                },
                required: ["type", "path", "subdirectory"],
              },
              {
                type: "object",
                additionalProperties: false,
                properties: {
                  type: { type: "string", enum: ["git"] },
                  repository: { type: "string" },
                  commit: { type: "string" },
                  subdirectory: { type: ["string", "null"] },
                },
                required: ["type", "repository", "commit", "subdirectory"],
              },
              { type: "null" },
            ],
            description:
              "A workspace-relative local source {type:'local',path} or pinned Git source {type:'git',repository,commit,subdirectory?}.",
          },
          initialization: {
            type: ["string", "null"],
            description:
              'Optional JSON-encoded initialization object. Use null for defaults. For secrets, pass a "${ENV_VAR}" reference (resolved from the host environment when the tool loads) instead of a literal value.',
          },
        },
        required: ["action", "toolId", "source", "initialization"],
      },
    },
    handler: {
      execute(args, execution) {
        return manageTool(execution.context, args);
      },
    },
  };
}

async function inspectTools(
  registry: HarnessToolRegistry,
  context: TurnContext,
  args: JsonObject,
): Promise<ToolResult> {
  rejectUnknownArguments(args, ["source", "operation", "toolId"]);
  const source = stringArgument(args, "source");
  const operation = stringArgument(args, "operation");
  if (source !== "active" && source !== "installed") {
    throw new Error("inspect_tools source must be active or installed");
  }
  if (operation !== "list" && operation !== "get") {
    throw new Error("inspect_tools operation must be list or get");
  }
  const toolId = operation === "get" ? stringArgument(args, "toolId") : null;
  if (operation !== "get" && args.toolId != null) {
    throw new Error("inspect_tools toolId is only valid for get");
  }

  if (source === "active") {
    const tools = registry.instances().map(activeToolSummary);
    if (toolId !== null) {
      const tool = tools.find(
        (candidate) => candidate.toolId === toolId || candidate.name === toolId,
      );
      return { ok: true, source, tool: tool ?? null };
    }
    return { ok: true, source, tools };
  }

  const snapshot = await readToolRegistry();
  const tools = await installedToolSummaries(context, snapshot.installed);
  if (toolId !== null) {
    const tool = tools.find((candidate) => candidate.toolId === toolId);
    return { ok: true, source, tool: tool ?? null };
  }
  return { ok: true, source, tools };
}

async function manageTool(
  context: TurnContext,
  args: JsonObject,
): Promise<ToolResult> {
  rejectUnknownArguments(args, [
    "action",
    "toolId",
    "source",
    "initialization",
  ]);
  const action = stringArgument(args, "action");
  if (action === "remove") {
    rejectPresentArguments(args, ["source", "initialization"]);
    const toolId = stringArgument(args, "toolId");
    const removed = await removeInstalledTool(toolId);
    return { ok: true, action, removed: removed !== null, toolId };
  }
  if (action !== "install") {
    throw new Error("manage_tool action must be install or remove");
  }

  if (args.toolId != null) {
    throw new Error("manage_tool toolId is not valid for install");
  }
  if (args.source == null) {
    throw new Error("manage_tool source is required");
  }
  const source = parseToolSource(args.source);
  const initialization =
    args.initialization == null
      ? {}
      : parseInitializationArgument(args.initialization);
  const installed = await installToolSource(
    {
      source,
      initialization,
    },
    async (modulePath, candidateInitialization) => {
      const tool = await loadAgentTool(
        context,
        modulePath,
        candidateInitialization,
      );
      assertInstallableToolName(tool.definition.name);
      return { toolName: tool.definition.name };
    },
  );
  return {
    ok: true,
    action,
    installed: await installedToolSummary(context, installed),
    availableNextRound: true,
  };
}

function activeToolSummary(tool: ToolInstance): JsonObject {
  return {
    toolId:
      tool.source === "built_in"
        ? `built_in:${tool.definition.name}`
        : `active:${tool.definition.name}`,
    name: tool.definition.name,
    description: tool.definition.description,
    source: tool.source,
    definition: tool.definition as unknown as JsonObject,
  };
}

async function installedToolSummaries(
  context: TurnContext,
  installedTools: InstalledTool[],
): Promise<JsonObject[]> {
  const summaries: JsonObject[] = [];
  for (const installed of installedTools) {
    try {
      summaries.push(await installedToolSummary(context, installed));
    } catch (error) {
      console.error(
        `skipping broken installed tool ${installed.id}: ${errorMessage(error)}`,
      );
    }
  }
  return summaries;
}

async function installedToolSummary(
  context: TurnContext,
  installed: InstalledTool,
): Promise<JsonObject> {
  const tool = await loadAgentTool(
    context,
    await installedToolModulePath(installed),
    installed.initialization,
  );
  return {
    toolId: installed.id,
    name: tool.definition.name,
    source: sourceSummary(installed.source),
  };
}

function sourceSummary(source: ToolSource): JsonObject {
  if (source.type === "local") {
    return {
      type: source.type,
      path: source.path,
      ...(source.subdirectory === undefined
        ? {}
        : { subdirectory: source.subdirectory }),
    };
  }
  return {
    type: source.type,
    repository: source.repository,
    commit: source.commit,
    ...(source.subdirectory === undefined
      ? {}
      : { subdirectory: source.subdirectory }),
  };
}

function parseInitializationArgument(value: unknown): JsonObject {
  if (typeof value !== "string") {
    throw new Error("manage_tool initialization must be a JSON string or null");
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(value) as unknown;
  } catch {
    throw new Error("manage_tool initialization must contain valid JSON");
  }
  return parseInitialization(parsed as JsonObject);
}

function assertInstallableToolName(name: string): void {
  if (
    name === "shell" ||
    name === "inspect_tools" ||
    name === "manage_tool" ||
    name === "install_agent_tool" ||
    name === "uninstall_agent_tool"
  ) {
    throw new Error(`installed tool cannot replace built-in tool: ${name}`);
  }
}

function createInstallAgentToolInstance(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "install_agent_tool",
      description:
        "Install or replace an agent-created TypeScript tool so it can be used in the next model round. The moduleSource must use type-only imports from @exo/harness/tool and default-export a Tool. Do not import external npm packages; use Node built-ins and global APIs like fetch.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description:
              "Filesystem-safe tool module name, for example curl-tool or grab_webpage.",
          },
          moduleSource: {
            type: "string",
            description:
              "Complete TypeScript source for a module that default-exports { definition, initializationParameters, initialize(...) } satisfies Tool. Do not use zod, inputSchema, outputSchema validators, call(...) handlers, or runtime imports from @exo/harness/tool.",
          },
          initialization: {
            type: "object",
            additionalProperties: false,
            properties: {
              apiKeyEnv: {
                type: ["string", "null"],
                description:
                  "Optional environment variable name containing an API key for generated tools that need one.",
              },
            },
            required: ["apiKeyEnv"],
            description:
              "Initialization arguments for the tool's initializationParameters schema.",
          },
        },
        required: ["name", "moduleSource", "initialization"],
      },
      outputSchema: {
        type: "object",
        additionalProperties: false,
        properties: {
          ok: { type: "boolean" },
          toolName: { type: "string" },
          modulePath: { type: "string" },
          availableNextRound: { type: "boolean" },
        },
        required: ["ok", "toolName", "modulePath", "availableNextRound"],
      },
    },
    handler: {
      execute(args, execution) {
        return installAgentTool(execution.context, args);
      },
    },
  };
}

function createUninstallAgentToolInstance(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "uninstall_agent_tool",
      description:
        "Remove an agent-created tool that was previously installed with install_agent_tool, so it is no longer registered in later model rounds. The name is the module name used at install time.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description:
              "Module name passed to install_agent_tool, for example curl-tool or grab_webpage.",
          },
        },
        required: ["name"],
      },
      outputSchema: {
        type: "object",
        additionalProperties: false,
        properties: {
          ok: { type: "boolean" },
          removed: { type: "boolean" },
          modulePath: { type: "string" },
        },
        required: ["ok", "removed", "modulePath"],
      },
    },
    handler: {
      execute(args) {
        return uninstallAgentTool(args);
      },
    },
  };
}

async function uninstallAgentTool(args: JsonObject): Promise<ToolResult> {
  const name = stringArgument(args, "name");
  if (!/^[A-Za-z0-9_-]+$/.test(name)) {
    throw new Error(
      "agent tool name must contain only letters, numbers, underscores, and dashes",
    );
  }
  const toolsDirectory = DEFAULT_AGENT_TOOL_DIRECTORY;
  const modulePath = path.join(toolsDirectory, `${name}.ts`);
  const sourcePath = path.join(toolsDirectory, `${name}.source.ts`);
  const existed = await fs
    .access(modulePath)
    .then(() => true)
    .catch(() => false);
  await fs.rm(modulePath, { force: true });
  await fs.rm(sourcePath, { force: true });
  return {
    ok: true,
    removed: existed,
    modulePath,
  };
}

async function installAgentTool(
  context: TurnContext,
  args: JsonObject,
): Promise<ToolResult> {
  const name = stringArgument(args, "name");
  if (!/^[A-Za-z0-9_-]+$/.test(name)) {
    throw new Error(
      "agent tool name must contain only letters, numbers, underscores, and dashes",
    );
  }
  const moduleSource = stringArgument(args, "moduleSource");
  const initialization = compactInitialization(
    objectArgument(args, "initialization"),
  );
  const toolsDirectory = DEFAULT_AGENT_TOOL_DIRECTORY;
  const modulePath = path.join(toolsDirectory, `${name}.ts`);
  const sourcePath = path.join(toolsDirectory, `${name}.source.ts`);
  const tempDirectory = path.join(toolsDirectory, ".tmp");
  const tempId = `${process.pid}.${Date.now()}`;
  const tempSourcePath = path.join(
    tempDirectory,
    `${name}.${tempId}.source.ts`,
  );
  const tempModulePath = path.join(tempDirectory, `${name}.${tempId}.ts`);

  await fs.mkdir(toolsDirectory, { recursive: true });
  await fs.mkdir(tempDirectory, { recursive: true });
  let tool: ToolInstance | null = null;
  try {
    await fs.writeFile(tempSourcePath, moduleSource, "utf8");
    await fs.writeFile(
      tempModulePath,
      agentToolWrapperSource(
        `./${path.basename(tempSourcePath)}`,
        initialization,
      ),
      "utf8",
    );
    tool = await loadAgentTool(context, path.resolve(tempModulePath));
    if (
      tool.definition.name === "shell" ||
      tool.definition.name === "install_agent_tool" ||
      tool.definition.name === "uninstall_agent_tool"
    ) {
      throw new Error(
        `agent tool cannot replace built-in tool: ${tool.definition.name}`,
      );
    }
  } finally {
    await fs.rm(tempSourcePath, { force: true });
    await fs.rm(tempModulePath, { force: true });
  }
  if (!tool) {
    throw new Error("agent tool validation did not return a tool");
  }

  const conflictingModulePath = await findAgentToolNameConflict(
    tool.definition.name,
    name,
    toolsDirectory,
  );
  if (conflictingModulePath) {
    const conflictingModuleName = path.basename(conflictingModulePath, ".ts");
    throw new Error(
      `tool ${tool.definition.name} is already installed by module ${conflictingModuleName}; reinstall using name ${conflictingModuleName} to replace it, or uninstall_agent_tool it first`,
    );
  }

  await fs.writeFile(sourcePath, moduleSource, "utf8");
  await fs.writeFile(
    modulePath,
    agentToolWrapperSource(`./${path.basename(sourcePath)}`, initialization),
    "utf8",
  );

  return {
    ok: true,
    toolName: tool.definition.name,
    modulePath,
    availableNextRound: true,
  };
}

function agentToolWrapperSource(
  sourceModulePath: string,
  initialization: JsonObject,
): string {
  return `import tool from ${JSON.stringify(sourceModulePath)};

export default {
  tool,
  initialization: ${JSON.stringify(initialization, null, 2)},
};
`;
}

function stringArgument(args: JsonObject, name: string): string {
  const value = args[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`tool argument ${name} must be a non-empty string`);
  }
  return value;
}

function rejectUnknownArguments(args: JsonObject, allowed: string[]): void {
  const unknown = Object.keys(args).find((key) => !allowed.includes(key));
  if (unknown) {
    throw new Error(`tool argument ${unknown} is not allowed`);
  }
}

function rejectPresentArguments(args: JsonObject, names: string[]): void {
  const present = names.find((name) => args[name] != null);
  if (present) {
    throw new Error(`tool argument ${present} is not valid for this action`);
  }
}

function objectArgument(args: JsonObject, name: string): JsonObject {
  const value = args[name];
  if (!isRecord(value)) {
    throw new Error(`tool argument ${name} must be an object`);
  }
  return value;
}

function compactInitialization(initialization: JsonObject): JsonObject {
  return Object.fromEntries(
    Object.entries(initialization).filter(([, value]) => value !== null),
  );
}

function isRecord(value: unknown): value is JsonObject {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
