import type { ToolModuleExport } from "./tool-modules";

export type JsonPrimitive = string | number | boolean | null;
export type JsonValue = JsonPrimitive | JsonObject | JsonValue[];
export interface JsonObject {
  [key: string]: JsonValue;
}

export * from "./tools";
export * from "./built-in-tools";
export * from "./tool-modules";
export * from "./adapter-tools";
export * from "./skill-tools";

export type MessageRole =
  | "system"
  | "developer"
  | "user"
  | "assistant"
  | "tool";

export interface Message {
  role: MessageRole;
  content: unknown;
  id?: string | null;
}

export interface AgentConfig {
  instructions: Message[];
  harness: "basic" | "rlm" | "typescript" | "exo";
  typescript?: {
    modulePath: string;
    toolModulePaths: string[];
  } | null;
  enableAgentToolCreation: boolean;
  sandbox: AgentSandboxConfig;
  model: string;
  maxOutputTokens?: number | null;
  maxToolRoundTrips?: number | null;
  braintrust?: unknown;
}

export interface AgentSandboxConfig {
  image?: string | null;
  provider: "daytona" | "apple_container" | "docker" | "local_process";
  mounts: FileSystemMount[];
  enableNetworking: boolean;
  scope: "agent" | "conversation";
}

export type Binding =
  | {
      type: "env";
      name: string;
      envVar: string;
      secretId: string;
    }
  | {
      type: "mcp";
      name: string;
      serverUrl: string;
      secretId?: string | null;
    }
  | {
      type: "llm";
      name: string;
      model: string;
      provider?: string | null;
      baseUrl?: string | null;
      secretId?: string | null;
    };

export interface BindingRecord {
  id: string;
  type: "env" | "mcp" | "llm";
  name: string;
  createdAt: string;
  binding: Binding;
}

export type Secret =
  | {
      type: "key";
      value: string;
    }
  | {
      type: "oauth";
      provider?: string | null;
      accountId?: string | null;
      accessToken?: string | null;
      refreshToken?: string | null;
      expiresAt?: string | null;
    };

export interface LogoutOauthResult {
  wasLoggedIn: boolean;
  remoteRevocationConfirmed: boolean;
}

export interface SecretMetadata {
  id: string;
  type: "key" | "oauth";
  name: string;
  createdAt: string;
}

export interface ConversationConfig {
  sandboxImage?: string | null;
  sandboxProvider?:
    | "daytona"
    | "apple_container"
    | "docker"
    | "local_process"
    | null;
  shellProgram?: string | null;
  sandboxScope?: "agent" | "conversation" | null;
  mounts: FileSystemMount[];
}

export interface FileSystemMount {
  hostPath: string;
  mountPath: string;
  mode: "ro" | "rw";
  internal?: boolean | null;
}

export interface ToolDefinition {
  name: string;
  description: string;
  parameters: JsonValue;
  outputSchema?: JsonValue;
}

export interface ToolRequest {
  functionName: string;
  arguments: JsonObject;
}

export interface SandboxProcessStartRequest {
  command: string[];
  env?: Record<string, string>;
  reuseKey?: string;
}

export interface SandboxProcess {
  readonly sandboxId?: string;
  readonly sandboxProcessId?: string;
  readonly reused: boolean;
  readonly stdout: ReadableStream<string>;
  readonly stderr: ReadableStream<string>;
  writeStdin(data: string): Promise<void>;
  closeStdin(): Promise<void>;
  close(): Promise<void>;
  wait(): Promise<number | null>;
}

export interface PendingToolCall {
  toolCallId: string;
  request: ToolRequest;
}

export interface SendRequest {
  input: Message[];
  sessionId?: string | null;
}

export interface AgentRecord {
  id: string;
  slug: string;
  name: string;
}

export interface ConversationRecord {
  id: string;
  slug: string;
  name: string;
  latestEventId?: string | null;
}

export interface TurnRecord {
  id: string;
  sessionId: string;
}

export interface ArtifactVersion {
  artifactId: string;
  path: string;
  version: number;
  createdAt: string;
  sizeBytes: number;
}

export interface Artifact extends ArtifactVersion {
  contents: Uint8Array;
}

export type EventQueryDirection = "asc" | "desc";

export interface EventQuery {
  cursor?: string | null;
  direction?: EventQueryDirection | null;
  limit?: number | null;
  sessionId?: string | null;
  turnId?: string | null;
  types?: string[] | null;
}

export interface GetEventsResult {
  events: Event[];
  cursor?: string | null;
}

export interface AddEventsRequest {
  sessionId?: string | null;
  turnId?: string | null;
  data: EventData[];
}

export interface AddEventsResult {
  eventIds: string[];
  latestEventId: string;
}

export interface NewConversationRequest {
  slug?: string | null;
  name?: string | null;
}

export interface ForkConversationRequest {
  upToInclusive?: string | null;
  slug?: string | null;
  name?: string | null;
}

export type EventData = { type: string } & Record<string, unknown>;
export interface Event {
  id: string;
  conversationId: string;
  sessionId?: string | null;
  turnId?: string | null;
  createdAt: string;
  data: EventData;
}

export type ToolResult = JsonValue;

export interface HistoryMessage {
  index: number;
  role: MessageRole;
  content: string;
}

export interface Agent {
  readonly record: AgentRecord;
  listConversations(): Promise<Conversation[]>;
  getConversation(id: string): Promise<Conversation | null>;
  newConversation(request?: NewConversationRequest): Promise<Conversation>;
  deleteConversation(id: string): Promise<boolean>;
  listArtifacts(): Promise<ArtifactVersion[]>;
  readArtifact(args: {
    artifactId: string;
    version?: number;
  }): Promise<Artifact | null>;
  readArtifactText(args: {
    artifactId: string;
    version?: number;
  }): Promise<string | null>;
  readArtifactJson<T>(args: {
    artifactId: string;
    version?: number;
  }): Promise<T | null>;
  writeArtifact(args: {
    path: string;
    contents: Uint8Array | string;
  }): Promise<ArtifactVersion>;
  writeArtifactText(args: {
    path: string;
    text: string;
  }): Promise<ArtifactVersion>;
  writeArtifactJson(args: {
    path: string;
    value: JsonValue;
  }): Promise<ArtifactVersion>;
  listBindings(): Promise<BindingRecord[]>;
  getBinding(id: string): Promise<Binding | null>;
  listSecrets(): Promise<SecretMetadata[]>;
  getSecret(id: string): Promise<Secret | null>;
}

export interface ExoHarness {
  readonly current: ExoHarnessCurrent;
  preflightSecretStorage(): Promise<void>;
  listAgents(): Promise<Agent[]>;
  getAgent(id: string): Promise<Agent | null>;
  newAgent(request: { slug: string; name: string }): Promise<Agent>;
  deleteAgent(id: string): Promise<boolean>;
  listBindings(): Promise<BindingRecord[]>;
  getBinding(id: string): Promise<Binding | null>;
  listSecrets(): Promise<SecretMetadata[]>;
  getSecret(id: string): Promise<Secret | null>;
  logoutOauthSecret(id: string): Promise<LogoutOauthResult>;
}

export interface ExoHarnessCurrent {
  readonly agent: Agent;
  readonly conversation: Conversation;
  readonly turn: Turn;
}

export interface Conversation {
  readonly agentId: string;
  readonly record: ConversationRecord;
  startSession(): Promise<string>;
  endSession(id: string): Promise<void>;
  getEvents(query?: EventQuery): Promise<GetEventsResult>;
  getEvent(id: string): Promise<Event | null>;
  addEvents(request: AddEventsRequest): Promise<AddEventsResult>;
  fork(request?: ForkConversationRequest): Promise<Conversation>;
  listArtifacts(): Promise<ArtifactVersion[]>;
  readArtifact(args: {
    artifactId: string;
    version?: number;
  }): Promise<Artifact | null>;
  readArtifactText(args: {
    artifactId: string;
    version?: number;
  }): Promise<string | null>;
  readArtifactJson<T>(args: {
    artifactId: string;
    version?: number;
  }): Promise<T | null>;
  writeArtifact(args: {
    path: string;
    contents: Uint8Array | string;
  }): Promise<ArtifactVersion>;
  writeArtifactText(args: {
    path: string;
    text: string;
  }): Promise<ArtifactVersion>;
  writeArtifactJson(args: {
    path: string;
    value: JsonValue;
  }): Promise<ArtifactVersion>;
  listBindings(): Promise<BindingRecord[]>;
  getBinding(id: string): Promise<Binding | null>;
  listSecrets(): Promise<SecretMetadata[]>;
  getSecret(id: string): Promise<Secret | null>;
}

export interface Turn {
  readonly agentId: string;
  readonly conversationId: string;
  readonly sessionId: string;
  readonly turnId: string;
  readonly conversation: Conversation;
  readonly record: TurnRecord;
  addEvents(data: EventData[]): Promise<AddEventsResult>;
  writeArtifact(args: {
    path: string;
    contents: Uint8Array | string;
  }): Promise<ArtifactVersion>;
  writeArtifactText(args: {
    path: string;
    text: string;
  }): Promise<ArtifactVersion>;
  writeArtifactJson(args: {
    path: string;
    value: JsonValue;
  }): Promise<ArtifactVersion>;
}

export interface TurnContext {
  readonly agentConfig: AgentConfig;
  readonly conversationConfig: ConversationConfig;
  readonly request: SendRequest;
  readonly streaming: boolean;
  readonly braintrustParent?: string | null;
  readonly exoharness: ExoHarness;
  executeTool(request: ToolRequest): Promise<ToolResult>;
  startSandboxProcess(
    request: SandboxProcessStartRequest,
  ): Promise<SandboxProcess>;
  executePendingTools(toolCalls: PendingToolCall[]): Promise<EventData[]>;
  stream: {
    firstChunk(ttftMs: number): Promise<void>;
    text(text: string): Promise<void>;
    toolCall(args: {
      toolCallId: string;
      toolName: string;
      arguments: JsonObject;
    }): Promise<void>;
    toolResult(args: { toolCallId: string; result: ToolResult }): Promise<void>;
  };
}

export interface TypeScriptHarness {
  tools?: ToolModuleExport;
  runTurn(context: TurnContext): Promise<void>;
}

export function defineHarness(harness: TypeScriptHarness): TypeScriptHarness {
  return harness;
}

export function turnMetadata(
  context: TurnContext,
  extra: Record<string, string> = {},
): Record<string, string> {
  const { agent, conversation, turn } = context.exoharness.current;
  return {
    agent_id: agent.record.id,
    conversation_id: conversation.record.id,
    turn_id: turn.record.id,
    ...extra,
  };
}

export function assertRoundBudget(
  context: TurnContext,
  round: number,
  label: string,
): void {
  const maxToolRoundTrips = context.agentConfig.maxToolRoundTrips;
  if (
    maxToolRoundTrips !== null &&
    maxToolRoundTrips !== undefined &&
    round > maxToolRoundTrips
  ) {
    throw new Error(`${label} exceeded the configured round budget`);
  }
}

export function systemTextMessage(text: string): Message {
  return {
    role: "system",
    content: text,
  };
}

export function userTextMessage(text: string): Message {
  return {
    role: "user",
    content: text,
  };
}

export function assistantTextMessage(text: string): Message {
  return {
    role: "assistant",
    content: text,
  };
}

export function messagesEvent(
  messages: Message[],
  responseId?: string,
  usage?: JsonObject,
): EventData {
  return {
    type: "messages",
    messages,
    response_id: responseId,
    ...(usage ? { usage } : {}),
  };
}

export function toolRequestedEvent(
  toolCall: PendingToolCall,
  responseId?: string,
): EventData {
  return {
    type: "tool_requested",
    tool_call_id: toolCall.toolCallId,
    response_id: responseId,
    request: {
      function_name: toolCall.request.functionName,
      arguments: toolCall.request.arguments,
    },
  };
}

export function toolResultEvent(
  toolCallId: string,
  result: ToolResult,
): EventData {
  return {
    type: "tool_result",
    tool_call_id: toolCallId,
    result,
  };
}

export function projectAnthropicMessageToolEvents(
  message: unknown,
  options: { toolNamePrefix?: string } = {},
): EventData[] {
  const record = recordOrEmpty(message);
  const payload = recordOrEmpty(record.message);
  const content = Array.isArray(payload.content) ? payload.content : [];
  const events: EventData[] = [];

  if (record.type === "assistant") {
    for (const block of content) {
      const toolUse = recordOrEmpty(block);
      if (
        toolUse.type === "tool_use" &&
        typeof toolUse.id === "string" &&
        typeof toolUse.name === "string"
      ) {
        events.push(
          toolRequestedEvent({
            toolCallId: toolUse.id,
            request: {
              functionName: `${options.toolNamePrefix ?? ""}${toolUse.name}`,
              arguments: isRecord(toolUse.input)
                ? (toJsonValue(toolUse.input) as JsonObject)
                : {},
            },
          }),
        );
      }
    }
  } else if (record.type === "user") {
    for (const block of content) {
      const toolResult = recordOrEmpty(block);
      if (
        toolResult.type === "tool_result" &&
        typeof toolResult.tool_use_id === "string"
      ) {
        events.push(
          toolResultEvent(
            toolResult.tool_use_id,
            toJsonValue({
              content: toolResult.content ?? null,
              is_error:
                typeof toolResult.is_error === "boolean"
                  ? toolResult.is_error
                  : false,
            }),
          ),
        );
      }
    }
  }

  return events;
}

export async function appendMessages(
  turn: Turn,
  messages: Message[],
  responseId?: string,
): Promise<AddEventsResult> {
  return turn.addEvents([messagesEvent(messages, responseId)]);
}

export async function appendCustomEvent(
  turn: Turn,
  eventType: string,
  payload: unknown,
): Promise<AddEventsResult> {
  return turn.addEvents([
    {
      type: "custom",
      event_type: eventType,
      payload: toJsonValue(payload),
    },
  ]);
}

export async function replyText(
  turn: Turn,
  text: string,
  responseId?: string,
): Promise<AddEventsResult> {
  return appendMessages(turn, [assistantTextMessage(text)], responseId);
}

export async function getMessages(
  conversation: Conversation,
  query?: EventQuery,
): Promise<Message[]> {
  const result = await conversation.getEvents(query);
  const messages: Message[] = [];
  for (const event of result.events) {
    if (
      event.data.type === "messages" &&
      Array.isArray((event.data as { messages?: unknown }).messages)
    ) {
      messages.push(
        ...((event.data as unknown as { messages: Message[] }).messages ?? []),
      );
    }
  }
  return messages;
}

export async function materializeConversationMessages(
  conversation: Conversation,
): Promise<Message[]> {
  const result = await conversation.getEvents({
    direction: "asc",
    types: ["messages", "tool_requested", "tool_result"],
  });
  return materializeEventsToMessages(result.events);
}

export function materializeEventsToMessages(events: Event[]): Message[] {
  const messages: Message[] = [];
  const toolCallNames = new Map<string, string>();
  const pendingToolCallIds: string[] = [];

  for (const event of events) {
    extendMaterializedMessages(
      messages,
      toolCallNames,
      pendingToolCallIds,
      event,
    );
  }
  flushDanglingToolResults(messages, toolCallNames, pendingToolCallIds);

  return messages;
}

export async function materializePromptMessages(
  conversation: Conversation,
  instructions: Message[],
): Promise<Message[]> {
  return [
    ...instructions,
    ...(await materializeConversationMessages(conversation)),
  ];
}

export function messagesToHistoryMessages(
  messages: Message[],
): HistoryMessage[] {
  return messages.map((message, index) => ({
    index,
    role: message.role,
    content: messageText(message),
  }));
}

export function messagesToTranscript(messages: Message[]): string {
  return messagesToHistoryMessages(messages)
    .map((message) => `${message.role.toUpperCase()}:\n${message.content}`)
    .join("\n\n");
}

export function assistantMessagesText(messages: Message[]): string {
  return messages
    .filter((message) => message.role === "assistant")
    .map(messageText)
    .join("\n");
}

export function toolResultMessage(
  toolCallId: string,
  toolName: string,
  output: ToolResult,
): Message {
  return {
    role: "tool",
    content: [
      {
        type: "tool_result",
        tool_call_id: toolCallId,
        tool_name: toolName,
        output,
      },
    ],
  };
}

export function filterMessages(
  messages: Message[],
  role?: MessageRole,
): Message[] {
  if (!role) {
    return [...messages];
  }
  return messages.filter((message) => message.role === role);
}

export function lastMessage(
  messages: Message[],
  role?: MessageRole,
): Message | undefined {
  const filtered = filterMessages(messages, role);
  return filtered.at(-1);
}

export function messageText(message: Message | null | undefined): string {
  if (!message) {
    return "";
  }
  return contentText(message.content);
}

function contentText(content: unknown): string {
  if (typeof content === "string") {
    return content;
  }
  if (!Array.isArray(content)) {
    return "";
  }
  return content
    .map((part) => {
      if (
        part &&
        typeof part === "object" &&
        "type" in part &&
        (part as { type?: unknown }).type === "text" &&
        "text" in part &&
        typeof (part as { text?: unknown }).text === "string"
      ) {
        return (part as { text: string }).text;
      }
      if (
        part &&
        typeof part === "object" &&
        "type" in part &&
        (part as { type?: unknown }).type === "image"
      ) {
        return "[image]";
      }
      if (
        part &&
        typeof part === "object" &&
        "type" in part &&
        (part as { type?: unknown }).type === "reasoning" &&
        "text" in part &&
        typeof (part as { text?: unknown }).text === "string"
      ) {
        return `[reasoning] ${(part as { text: string }).text}`;
      }
      if (
        part &&
        typeof part === "object" &&
        "type" in part &&
        (part as { type?: unknown }).type === "tool_result" &&
        "tool_name" in part &&
        "output" in part
      ) {
        return `${String((part as { tool_name?: unknown }).tool_name)} => ${stringifyValue((part as { output?: unknown }).output)}`;
      }
      if (
        part &&
        typeof part === "object" &&
        "type" in part &&
        (part as { type?: unknown }).type === "tool_call" &&
        "tool_name" in part &&
        "arguments" in part
      ) {
        return `[tool_call ${String((part as { tool_name?: unknown }).tool_name)}] ${stringifyValue((part as { arguments?: unknown }).arguments)}`;
      }
      return "";
    })
    .join("");
}

function extendMaterializedMessages(
  messages: Message[],
  toolCallNames: Map<string, string>,
  pendingToolCallIds: string[],
  event: Event,
): void {
  if (isMessagesEvent(event.data)) {
    flushDanglingToolResults(messages, toolCallNames, pendingToolCallIds);
    messages.push(...event.data.messages);
    return;
  }

  if (isToolRequestedEvent(event.data)) {
    toolCallNames.set(
      event.data.tool_call_id,
      event.data.request.function_name,
    );
    pendingToolCallIds.push(event.data.tool_call_id);
    return;
  }

  if (isToolResultEvent(event.data)) {
    const toolName = toolCallNames.get(event.data.tool_call_id);
    if (!toolName) {
      return;
    }
    removePendingToolCall(pendingToolCallIds, event.data.tool_call_id);
    messages.push(
      toolResultMessage(event.data.tool_call_id, toolName, event.data.result),
    );
  }
}

function flushDanglingToolResults(
  messages: Message[],
  toolCallNames: Map<string, string>,
  pendingToolCallIds: string[],
): void {
  while (pendingToolCallIds.length > 0) {
    const toolCallId = pendingToolCallIds.shift();
    if (!toolCallId) {
      continue;
    }
    const toolName = toolCallNames.get(toolCallId);
    if (!toolName) {
      continue;
    }
    messages.push(
      toolResultMessage(toolCallId, toolName, {
        ok: false,
        error: "tool execution did not complete before the previous turn ended",
      }),
    );
  }
}

function removePendingToolCall(
  pendingToolCallIds: string[],
  toolCallId: string,
): void {
  const index = pendingToolCallIds.indexOf(toolCallId);
  if (index >= 0) {
    pendingToolCallIds.splice(index, 1);
  }
}

function isMessagesEvent(
  data: EventData,
): data is EventData & { type: "messages"; messages: Message[] } {
  return data.type === "messages" && Array.isArray(data.messages);
}

function isToolRequestedEvent(data: EventData): data is EventData & {
  type: "tool_requested";
  tool_call_id: string;
  request: { function_name: string; arguments: JsonObject };
} {
  if (data.type !== "tool_requested") {
    return false;
  }
  if (typeof data.tool_call_id !== "string") {
    return false;
  }
  if (!data.request || typeof data.request !== "object") {
    return false;
  }
  return (
    typeof (data.request as { function_name?: unknown }).function_name ===
    "string"
  );
}

function isToolResultEvent(data: EventData): data is EventData & {
  type: "tool_result";
  tool_call_id: string;
  result: ToolResult;
} {
  return data.type === "tool_result" && typeof data.tool_call_id === "string";
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function recordOrEmpty(value: unknown): Record<string, unknown> {
  return isRecord(value) ? value : {};
}

export function toJsonValue(value: unknown): JsonValue {
  return JSON.parse(JSON.stringify(value)) as JsonValue;
}

export function stringifyValue(value: unknown): string {
  if (typeof value === "string") {
    return value;
  }
  return JSON.stringify(value) ?? String(value);
}

export function asBytes(contents: Uint8Array | string): Uint8Array {
  if (typeof contents === "string") {
    return new TextEncoder().encode(contents);
  }
  return contents;
}
