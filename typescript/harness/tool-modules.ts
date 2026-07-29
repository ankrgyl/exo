import fs from "node:fs/promises";
import type { Dirent } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

import type { JsonObject, JsonValue, TurnContext } from "./index";
import {
  initializeTool,
  type HarnessToolRegistry,
  type HarnessToolSource,
  type Tool,
  type ToolInstance,
} from "./tools";
import { installedToolModulePath, readToolRegistry } from "./tool-registry";

export * from "./tool-registry";

export const DEFAULT_AGENT_TOOL_DIRECTORY = ".exo/agent-tools";
let agentToolImportVersion = 0;

export interface ToolModule {
  tools: ToolModuleEntry[];
}

export interface ToolModuleEntry {
  tool: Tool;
  initialization?: JsonObject;
}

export type ToolModuleExport =
  | Tool
  | ToolModuleEntry
  | ToolModule
  | Array<Tool | ToolModuleEntry | ToolModule>;

export function defineToolModule(module: ToolModule): ToolModule {
  return module;
}

export function defineToolModuleEntry(entry: ToolModuleEntry): ToolModuleEntry {
  return entry;
}

export async function registerTools(
  registry: HarnessToolRegistry,
  context: TurnContext,
  exported: ToolModuleExport,
  source: Extract<HarnessToolSource, "library" | "agent">,
): Promise<void> {
  for (const entry of normalizeToolModuleExport(exported, source)) {
    registry.register(await initializeToolModuleEntry(context, entry, source));
  }
}

export function registerLibraryTools(
  registry: HarnessToolRegistry,
  context: TurnContext,
  exported: ToolModuleExport,
): Promise<void> {
  return registerTools(registry, context, exported, "library");
}

export function registerAgentTools(
  registry: HarnessToolRegistry,
  context: TurnContext,
  exported: ToolModuleExport,
): Promise<void> {
  return registerTools(registry, context, exported, "agent");
}

export async function registerToolModulePath(
  registry: HarnessToolRegistry,
  context: TurnContext,
  modulePath: string,
  source: Extract<HarnessToolSource, "library" | "agent">,
): Promise<void> {
  await registerTools(
    registry,
    context,
    await loadToolModule(modulePath, source),
    source,
  );
}

export function registerLibraryToolModulePath(
  registry: HarnessToolRegistry,
  context: TurnContext,
  modulePath: string,
): Promise<void> {
  return registerToolModulePath(registry, context, modulePath, "library");
}

export function registerAgentToolModulePath(
  registry: HarnessToolRegistry,
  context: TurnContext,
  modulePath: string,
): Promise<void> {
  return registerToolModulePath(registry, context, modulePath, "agent");
}

export async function registerAgentToolsFromDirectoryIfExists(
  registry: HarnessToolRegistry,
  context: TurnContext,
  toolsDirectory = DEFAULT_AGENT_TOOL_DIRECTORY,
): Promise<void> {
  await registerInstalledTools(registry, context);
  await registerLegacyAgentToolsFromDirectoryIfExists(
    registry,
    context,
    toolsDirectory,
  );
}

export async function registerInstalledTools(
  registry: HarnessToolRegistry,
  context: TurnContext,
): Promise<void> {
  const snapshot = await readToolRegistry();
  for (const installed of snapshot.installed) {
    try {
      const tool = await loadAgentTool(
        context,
        await installedToolModulePath(installed),
        installed.initialization,
      );
      registry.register(tool);
    } catch (error) {
      const message = errorMessage(error);
      console.error(
        `skipping broken installed tool ${installed.id}: ${message}`,
      );
    }
  }
}

export async function registerLegacyAgentToolsFromDirectoryIfExists(
  registry: HarnessToolRegistry,
  context: TurnContext,
  toolsDirectory = DEFAULT_AGENT_TOOL_DIRECTORY,
): Promise<void> {
  // A broken agent-written module (duplicate tool name, failed import, ...)
  // must not prevent the harness from starting: the agent cannot repair its
  // own tools if it can never boot. Skip the module and keep going.
  // Future TODO: consider self-repairing by removing the broken module.
  for (const modulePath of await agentToolModulePaths(toolsDirectory)) {
    try {
      await registerAgentToolModulePath(registry, context, modulePath);
    } catch (error) {
      console.error(
        `skipping agent tool module ${modulePath}: ${errorMessage(error)}`,
      );
    }
  }
}

export async function findAgentToolNameConflict(
  toolName: string,
  moduleName: string,
  toolsDirectory = DEFAULT_AGENT_TOOL_DIRECTORY,
): Promise<string | null> {
  const ownModulePath = path.resolve(toolsDirectory, `${moduleName}.ts`);
  for (const modulePath of await agentToolModulePaths(toolsDirectory)) {
    if (modulePath === ownModulePath) {
      continue;
    }
    let entries: ToolModuleEntry[];
    try {
      entries = normalizeToolModuleExport(
        await loadToolModule(modulePath, "agent"),
        "agent",
      );
    } catch {
      continue;
    }
    if (entries.some((entry) => entry.tool.definition.name === toolName)) {
      return modulePath;
    }
  }
  return null;
}

async function agentToolModulePaths(toolsDirectory: string): Promise<string[]> {
  let entries: Dirent[];
  try {
    entries = await fs.readdir(toolsDirectory, { withFileTypes: true });
  } catch (error) {
    if (isNotFoundError(error)) {
      return [];
    }
    throw error;
  }
  return entries
    .filter((entry) => entry.isFile())
    .map((entry) => entry.name)
    .filter((name) => name.endsWith(".ts") && !name.endsWith(".source.ts"))
    .sort()
    .map((name) => path.resolve(toolsDirectory, name));
}

export async function loadToolModule(
  modulePath: string,
  source: Extract<HarnessToolSource, "library" | "agent">,
): Promise<ToolModuleExport> {
  const module = (await import(importSpecifier(modulePath, source))) as Record<
    string,
    unknown
  >;
  const exported =
    module.default ?? module.toolModule ?? module.tool ?? module.tools;
  if (!exported) {
    throw new Error(
      `${source} tool module must export a Tool, ToolModuleEntry, or ToolModule: ${modulePath}`,
    );
  }
  if (module.tools && exported === module.tools) {
    return { tools: normalizeToolArray(module.tools, source) };
  }
  return normalizeToolModuleExport(exported, source);
}

export async function loadAgentTool(
  context: TurnContext,
  modulePath: string,
  initialization?: JsonObject,
): Promise<ToolInstance> {
  const entries = normalizeToolModuleExport(
    await loadToolModule(modulePath, "agent"),
    "agent",
  );
  if (entries.length !== 1) {
    throw new Error(
      `agent tool module must export exactly one tool: ${modulePath}`,
    );
  }
  return initializeToolModuleEntry(
    context,
    initialization === undefined
      ? entries[0]
      : { ...entries[0], initialization },
    "agent",
  );
}

function initializeToolModuleEntry(
  context: TurnContext,
  entry: ToolModuleEntry,
  source: Extract<HarnessToolSource, "library" | "agent">,
): Promise<ToolInstance> {
  const initialization =
    entry.initialization ?? entry.tool.initialization ?? {};
  return initializeTool(
    entry.tool,
    source,
    source === "agent"
      ? expandInitializationEnvironmentReferences(initialization)
      : initialization,
    context,
  );
}

// Agent tool initialization is persisted in the tool lockfile, so secrets must
// stay out of it. A string value of exactly "${NAME}" is resolved from the
// host environment when the tool loads; any other string passes through
// unchanged.
function expandInitializationEnvironmentReferences(
  initialization: JsonObject,
): JsonObject {
  return expandEnvironmentValue(initialization) as JsonObject;
}

function expandEnvironmentValue(value: JsonValue): JsonValue {
  if (typeof value === "string") {
    const match = /^\$\{([A-Za-z_][A-Za-z0-9_]*)\}$/.exec(value);
    if (!match) {
      return value;
    }
    const resolved = process.env[match[1]];
    if (resolved === undefined) {
      throw new Error(
        `tool initialization references environment variable ${match[1]}, which is not set`,
      );
    }
    return resolved;
  }
  if (Array.isArray(value)) {
    return value.map(expandEnvironmentValue);
  }
  if (isRecord(value)) {
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [
        key,
        expandEnvironmentValue(item),
      ]),
    );
  }
  return value;
}

function normalizeToolModuleExport(
  exported: unknown,
  source: Extract<HarnessToolSource, "library" | "agent">,
): ToolModuleEntry[] {
  if (Array.isArray(exported)) {
    return normalizeToolArray(exported, source);
  }
  if (isTool(exported)) {
    return [{ tool: exported }];
  }
  if (isToolModuleEntry(exported)) {
    return [exported];
  }
  if (isToolModule(exported)) {
    return normalizeToolArray(exported.tools, source);
  }
  throw new Error(
    `${source} tool module export must be a Tool, ToolModuleEntry, or ToolModule`,
  );
}

function normalizeToolArray(
  values: unknown,
  source: Extract<HarnessToolSource, "library" | "agent">,
): ToolModuleEntry[] {
  if (!Array.isArray(values)) {
    throw new Error(`${source} tool module tools export must be an array`);
  }
  return values.flatMap((value) => normalizeToolModuleExport(value, source));
}

function importSpecifier(
  modulePath: string,
  source: Extract<HarnessToolSource, "library" | "agent">,
): string {
  if (source !== "agent") {
    return modulePath;
  }
  if (modulePath.startsWith("data:")) {
    return modulePath;
  }
  const url = modulePath.startsWith("file:")
    ? new URL(modulePath)
    : path.isAbsolute(modulePath)
      ? pathToFileURL(modulePath)
      : null;
  if (!url) {
    return modulePath;
  }
  agentToolImportVersion += 1;
  url.searchParams.set("agentToolVersion", String(agentToolImportVersion));
  return url.href;
}

function isTool(value: unknown): value is Tool {
  if (!isRecord(value)) {
    return false;
  }
  const candidate = value as {
    definition?: unknown;
    initializationParameters?: unknown;
    initialize?: unknown;
  };
  return (
    Boolean(candidate.definition) &&
    Boolean(candidate.initializationParameters) &&
    typeof candidate.initialize === "function"
  );
}

function isToolModuleEntry(value: unknown): value is ToolModuleEntry {
  if (!isRecord(value) || !isTool(value.tool)) {
    return false;
  }
  if (value.initialization === undefined) {
    return true;
  }
  return isRecord(value.initialization);
}

function isToolModule(value: unknown): value is ToolModule {
  return isRecord(value) && Array.isArray(value.tools);
}

function isRecord(value: unknown): value is JsonObject {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function isNotFoundError(error: unknown): boolean {
  return (
    error !== null &&
    typeof error === "object" &&
    "code" in error &&
    (error as { code?: unknown }).code === "ENOENT"
  );
}
