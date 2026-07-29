import type {
  ArtifactVersion,
  EventData,
  JsonObject,
  JsonValue,
  PendingToolCall,
  ToolDefinition,
  ToolResult,
  TurnContext,
} from "./index";

export type HarnessToolSource = "built_in" | "library" | "agent";

const TOOL_RESULT_INLINE_LIMIT_CHARS = 8_000;
const TOOL_RESULT_PREVIEW_CHARS = 4_000;

export interface ToolExecutionContext {
  readonly context: TurnContext;
  readonly toolCallId?: string;
}

export interface ToolHandler {
  execute(
    args: JsonObject,
    execution: ToolExecutionContext,
  ): Promise<ToolResult>;
}

export interface ToolInstance {
  definition: ToolDefinition;
  source: HarnessToolSource;
  handler: ToolHandler;
}

export interface ToolInitializationContext {
  readonly context: TurnContext;
  readonly source: HarnessToolSource;
}

export interface Tool {
  definition: ToolDefinition;
  initializationParameters: JsonValue;
  initialization?: JsonObject;
  initialize(
    args: JsonObject,
    initialization: ToolInitializationContext,
  ): Promise<ToolHandler> | ToolHandler;
}

export function defineTool<T extends Tool>(tool: T): T {
  return tool;
}

export class HarnessToolRegistry {
  private readonly tools = new Map<string, ToolInstance>();

  constructor(private readonly context: TurnContext) {}

  register(tool: ToolInstance): this {
    const { name } = tool.definition;
    if (this.tools.has(name)) {
      throw new Error(`tool is already registered: ${name}`);
    }
    this.tools.set(name, tool);
    return this;
  }

  definitions(): ToolDefinition[] {
    return [...this.tools.values()].map((tool) => tool.definition);
  }

  get(name: string): ToolInstance | undefined {
    return this.tools.get(name);
  }

  async executePending(toolCalls: PendingToolCall[]): Promise<EventData[]> {
    const events: EventData[] = [];
    for (const toolCall of toolCalls) {
      const result = await this.executeToolCallOrError(toolCall);
      events.push(toolResultEvent(toolCall.toolCallId, result));
    }
    return events;
  }

  private async executeToolCallOrError(
    toolCall: PendingToolCall,
  ): Promise<ToolResult> {
    const configuredTool =
      this.tools.get(toolCall.request.functionName) ?? null;
    try {
      const { tool, result } = await this.executeToolCall(toolCall);
      return await this.normalizeAndStreamToolResult(toolCall, tool, result);
    } catch (error) {
      const result: ToolResult = {
        ok: false,
        error: errorMessage(error),
      };
      return await this.normalizeAndStreamToolResult(
        toolCall,
        configuredTool,
        result,
      );
    }
  }

  private async executeToolCall(
    toolCall: PendingToolCall,
  ): Promise<{ tool: ToolInstance; result: ToolResult }> {
    const tool = this.tools.get(toolCall.request.functionName);
    if (!tool) {
      throw new Error(
        `tool execution is not configured for ${toolCall.request.functionName}`,
      );
    }
    if (this.context.streaming) {
      await this.context.stream.toolCall({
        toolCallId: toolCall.toolCallId,
        toolName: toolCall.request.functionName,
        arguments: toolCall.request.arguments,
      });
    }
    const result = await tool.handler.execute(toolCall.request.arguments, {
      context: this.context,
      toolCallId: toolCall.toolCallId,
    });
    return { tool, result };
  }

  private async normalizeAndStreamToolResult(
    toolCall: PendingToolCall,
    tool: ToolInstance | null,
    result: ToolResult,
  ): Promise<ToolResult> {
    const normalized = await compactToolResult(this.context, {
      toolCallId: toolCall.toolCallId,
      toolName: tool?.definition.name ?? toolCall.request.functionName,
      source: tool?.source ?? "built_in",
      result,
    });
    if (this.context.streaming) {
      await this.context.stream.toolResult({
        toolCallId: toolCall.toolCallId,
        result: normalized,
      });
    }
    return normalized;
  }
}

interface CompactToolResultArgs {
  toolCallId: string;
  toolName: string;
  source: HarnessToolSource;
  result: ToolResult;
}

interface ToolResultArtifactReference extends JsonObject {
  artifactId: string;
  path: string;
  version: number;
  sizeBytes: number;
  mimeType: string;
}

async function compactToolResult(
  context: TurnContext,
  args: CompactToolResultArgs,
): Promise<ToolResult> {
  const fullResultArtifact = await writeToolResultArtifact(
    context,
    args,
    "result.json",
    `${JSON.stringify(args.result, null, 2)}\n`,
    "application/json",
  );
  const serialized = stringifyToolResult(args.result);
  const shellArtifacts = await writeShellOutputArtifacts(context, args);
  const value =
    serialized.length <= TOOL_RESULT_INLINE_LIMIT_CHARS ? args.result : null;
  return {
    ok: resultOk(args.result),
    toolName: args.toolName,
    toolCallId: args.toolCallId,
    source: args.source,
    resultArtifact: fullResultArtifact,
    artifacts: [fullResultArtifact, ...shellArtifacts],
    truncated: serialized.length > TOOL_RESULT_INLINE_LIMIT_CHARS,
    preview: previewText(serialized),
    value,
  };
}

async function writeShellOutputArtifacts(
  context: TurnContext,
  args: CompactToolResultArgs,
): Promise<ToolResultArtifactReference[]> {
  if (!isRecord(args.result)) {
    return [];
  }
  const artifacts: ToolResultArtifactReference[] = [];
  for (const key of ["stdout", "stderr"] as const) {
    const value = args.result[key];
    if (typeof value !== "string" || value.length === 0) {
      continue;
    }
    artifacts.push(
      await writeToolResultArtifact(
        context,
        args,
        `${key}.txt`,
        value,
        "text/plain",
      ),
    );
  }
  return artifacts;
}

async function writeToolResultArtifact(
  context: TurnContext,
  args: CompactToolResultArgs,
  fileName: string,
  text: string,
  mimeType: string,
): Promise<ToolResultArtifactReference> {
  const artifact = await context.exoharness.current.turn.writeArtifactText({
    path: `tool-results/${sanitizePathSegment(args.toolName)}/${sanitizePathSegment(args.toolCallId)}/${fileName}`,
    text,
  });
  return artifactReference(artifact, mimeType);
}

function artifactReference(
  artifact: ArtifactVersion,
  mimeType: string,
): ToolResultArtifactReference {
  return {
    artifactId: artifact.artifactId,
    path: artifact.path,
    version: artifact.version,
    sizeBytes: artifact.sizeBytes,
    mimeType,
  };
}

function resultOk(result: ToolResult): boolean {
  if (isRecord(result) && typeof result.ok === "boolean") {
    return result.ok;
  }
  return true;
}

function stringifyToolResult(result: ToolResult): string {
  return typeof result === "string" ? result : JSON.stringify(result, null, 2);
}

export function previewText(text: string): string {
  if (text.length <= TOOL_RESULT_PREVIEW_CHARS) {
    return text;
  }
  // Slicing by UTF-16 code units can cut a surrogate pair in half. A lone
  // high surrogate serializes as an unpaired \ud8xx escape, which strict JSON
  // parsers downstream reject — a single emoji at the boundary then fails the
  // whole turn. Drop the dangling half instead of emitting it.
  const cut = text
    .slice(0, TOOL_RESULT_PREVIEW_CHARS)
    .replace(/[\uD800-\uDBFF]$/, "");
  return `${cut}\n...[truncated]`;
}

function sanitizePathSegment(value: string): string {
  return value.replace(/[^A-Za-z0-9_.-]+/g, "_").slice(0, 96) || "unknown";
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function createToolRegistry(context: TurnContext): HarnessToolRegistry {
  return new HarnessToolRegistry(context);
}

export async function initializeTool(
  tool: Tool,
  source: HarnessToolSource,
  initializationArgs: JsonObject,
  context: TurnContext,
): Promise<ToolInstance> {
  validateToolDefinition(tool.definition);
  validateJsonSchema(
    tool.initializationParameters,
    initializationArgs,
    "tool initialization",
  );
  const handler = await tool.initialize(initializationArgs, {
    context,
    source,
  });
  validateToolHandler(handler);
  return {
    definition: tool.definition,
    source,
    handler,
  };
}

function validateToolDefinition(definition: ToolDefinition): void {
  if (typeof definition.name !== "string" || definition.name.length === 0) {
    throw new Error("tool definition.name must be a non-empty string");
  }
  if (
    !/^[A-Za-z0-9_-]+$/.test(definition.name) ||
    definition.name.length > 64
  ) {
    throw new Error(
      "tool definition.name must contain only letters, numbers, underscores, and dashes, and be at most 64 characters",
    );
  }
  if (
    typeof definition.description !== "string" ||
    definition.description.length === 0
  ) {
    throw new Error("tool definition.description must be a non-empty string");
  }
  const rawDefinition = definition as unknown as { inputSchema?: unknown };
  if (definition.parameters === undefined && rawDefinition.inputSchema) {
    throw new Error("tool definition must use parameters, not inputSchema");
  }
  if (!isRecord(definition.parameters)) {
    throw new Error("tool definition.parameters must be an object JSON schema");
  }
  if (definition.parameters.type !== "object") {
    throw new Error("tool definition.parameters.type must be object");
  }
  if (definition.parameters.additionalProperties !== false) {
    throw new Error(
      "tool definition.parameters.additionalProperties must be false",
    );
  }
}

function validateToolHandler(handler: ToolHandler): void {
  if (!handler || typeof handler !== "object") {
    throw new Error("tool initialize must return a handler object");
  }
  const candidate = handler as { execute?: unknown; invoke?: unknown };
  if (typeof candidate.execute !== "function" && candidate.invoke) {
    throw new Error("tool handler must implement execute, not invoke");
  }
  if (typeof candidate.execute !== "function") {
    throw new Error("tool handler must implement execute");
  }
}

function validateJsonSchema(
  schema: JsonValue,
  value: JsonValue,
  path: string,
): void {
  if (!isRecord(schema)) {
    return;
  }
  const type = schema.type;
  if (type !== undefined && !matchesJsonSchemaType(type, value)) {
    throw new Error(
      `${path} does not match schema type ${formatSchemaType(type)}`,
    );
  }
  if (type !== "object" || !isRecord(value)) {
    return;
  }
  const properties = isRecord(schema.properties) ? schema.properties : {};
  const required = Array.isArray(schema.required) ? schema.required : [];
  for (const requiredKey of required) {
    if (typeof requiredKey === "string" && !(requiredKey in value)) {
      throw new Error(`${path}.${requiredKey} is required`);
    }
  }
  if (schema.additionalProperties === false) {
    for (const key of Object.keys(value)) {
      if (!(key in properties)) {
        throw new Error(`${path}.${key} is not allowed`);
      }
    }
  }
  for (const [key, propertySchema] of Object.entries(properties)) {
    if (key in value) {
      validateJsonSchema(
        propertySchema as JsonValue,
        value[key],
        `${path}.${key}`,
      );
    }
  }
}

function matchesJsonSchemaType(type: JsonValue, value: JsonValue): boolean {
  if (Array.isArray(type)) {
    return type.some((candidate) => matchesJsonSchemaType(candidate, value));
  }
  if (type === "null") {
    return value === null;
  }
  if (type === "array") {
    return Array.isArray(value);
  }
  if (type === "object") {
    return isRecord(value);
  }
  if (type === "string") {
    return typeof value === "string";
  }
  if (type === "number") {
    return typeof value === "number";
  }
  if (type === "boolean") {
    return typeof value === "boolean";
  }
  return true;
}

function formatSchemaType(type: JsonValue): string {
  return Array.isArray(type) ? type.map(String).join(" | ") : String(type);
}

function isRecord(value: JsonValue): value is JsonObject {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function toolResultEvent(toolCallId: string, result: ToolResult): EventData {
  return {
    type: "tool_result",
    tool_call_id: toolCallId,
    result,
  };
}
