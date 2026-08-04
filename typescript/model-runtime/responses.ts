import OpenAI from "openai";
import Anthropic from "@anthropic-ai/sdk";
import {
  flush,
  initLogger,
  traced,
  wrapAnthropic,
  wrapOpenAI,
  type Span,
  type StartSpanArgs,
} from "braintrust";
import {
  linguaToAnthropicMessages,
  linguaToResponsesMessages,
  responsesMessagesToLingua,
  type Message as LinguaMessage,
} from "@braintrust/lingua";
import type {
  Response,
  ResponseCreateParamsNonStreaming,
  ResponseCreateParamsStreaming,
  ResponseInput,
  ResponseStreamEvent,
  Tool,
} from "openai/resources/responses/responses";

import {
  messagesEvent,
  toolResultEvent,
  toolRequestedEvent,
  type AgentConfig,
  type EventData,
  type JsonObject,
  type Message,
  type PendingToolCall,
  type ToolDefinition,
  type TurnContext,
} from "../harness";
import { computeCostUsd, getTable } from "./cost";
import type {
  ChatCompletion,
  ChatCompletionChunk,
  ChatCompletionCreateParamsNonStreaming,
  ChatCompletionCreateParamsStreaming,
  ChatCompletionMessageParam,
  ChatCompletionMessageToolCall,
  ChatCompletionTool,
} from "openai/resources/chat/completions";

export interface NativeBraintrustOptions {
  apiKey?: string;
  appUrl?: string;
  orgName?: string;
  projectName?: string;
  projectId?: string;
}

export interface ResponsesRuntimeOptions {
  apiKey?: string | null;
  // Bearer credential for the Anthropic SDK (`Authorization: Bearer ...`),
  // used when a provider speaks the Anthropic format but authenticates with
  // bearer auth. Pass null to also disable the ANTHROPIC_AUTH_TOKEN fallback.
  authToken?: string | null;
  baseURL?: string;
  organization?: string;
  project?: string;
  // A null value omits that header from every request.
  defaultHeaders?: Record<string, string | null>;
  // Path under the raw usage object where the provider reports spend in USD.
  costUsagePath?: string[] | null;
  braintrust?: NativeBraintrustOptions | null;
}

// Wire format a registered model provider speaks, mirroring the Rust
// `WireFormat` enum (kebab-case on the wire).
export type ProviderWireFormat = "chat-completions" | "responses" | "anthropic";

// How a registered provider authenticates, mirroring the Rust `AuthScheme`.
export type ProviderAuthScheme = "bearer" | "x-api-key" | "none";

export interface ResponsesModelBinding {
  model?: string;
  apiKey?: string;
  baseUrl?: string | null;
  // Declared wire format from a registered provider binding. When set it is
  // authoritative and skips the model-name / base-URL heuristics below.
  format?: ProviderWireFormat | null;
  // Declared auth scheme; absent = the wire format's native scheme.
  auth?: ProviderAuthScheme | null;
  // Where the provider reports spend under the response usage object.
  costUsagePath?: string[] | null;
}

// True when the binding came from a registered provider record. Such bindings
// carry their own credential (or are explicitly unauthenticated) and must
// never fall back to ambient env keys like OPENAI_API_KEY — with a custom
// base URL that would leak the ambient credential to the custom endpoint.
function isProviderDeclared(binding: ResponsesModelBinding): boolean {
  return binding.format != null || binding.auth != null;
}

// Placeholder that satisfies the OpenAI SDK's key requirement; the
// Authorization header itself is suppressed via a null default header, so it
// is never sent.
const UNAUTHENTICATED = "unauthenticated";

// Credentials for the OpenAI SDK honoring the provider's declared auth
// scheme. A null header value tells the SDK to omit that header entirely, so
// exactly the declared credential is sent — never a placeholder bearer, never
// both schemes at once.
export function openAiCredentials(binding: ResponsesModelBinding): {
  apiKey?: string;
  defaultHeaders?: Record<string, string | null>;
} {
  if (binding.auth === "none") {
    return {
      apiKey: UNAUTHENTICATED,
      defaultHeaders: { authorization: null },
    };
  }
  if (isProviderDeclared(binding) && !binding.apiKey) {
    // Mirror the Rust runtime: a provider either carries its own credential
    // or is explicitly unauthenticated — never silently unauthenticated.
    throw new Error(
      "model request is missing an API key (the provider has no secret)",
    );
  }
  if (binding.auth === "x-api-key" && binding.apiKey) {
    return {
      apiKey: UNAUTHENTICATED,
      defaultHeaders: { authorization: null, "x-api-key": binding.apiKey },
    };
  }
  return { apiKey: binding.apiKey };
}

// Reads the provider-reported spend from a raw usage object, following the
// provider record's declared cost path (cost is a vendor extension, not part
// of the standard usage schema).
// The Anthropic SDK strips unknown fields while accumulating the terminal
// message, so a vendor cost extension (e.g. usage.opper.cost.total) survives
// only on the raw stream events; capture it from message_start/message_delta.
export function costFromAnthropicStreamEvent(
  event: unknown,
  path: readonly string[],
): number | null {
  if (typeof event !== "object" || event === null) {
    return null;
  }
  const record = event as {
    type?: unknown;
    usage?: unknown;
    message?: { usage?: unknown };
  };
  if (record.type === "message_start") {
    return walkUsagePath(record.message?.usage, path);
  }
  if (record.type === "message_delta") {
    return walkUsagePath(record.usage, path);
  }
  return null;
}

export function walkUsagePath(
  raw: unknown,
  path: readonly string[],
): number | null {
  let node: unknown = raw;
  for (const segment of path) {
    if (typeof node !== "object" || node === null) {
      return null;
    }
    node = (node as Record<string, unknown>)[segment];
  }
  return typeof node === "number" ? node : null;
}

export interface NativeResponsesRequest {
  model: string;
  messages?: Message[];
  input?: string | ResponseInput;
  tools?: ToolDefinition[];
  responseTools?: Tool[];
  maxOutputTokens?: number | null;
  metadata?: Record<string, string>;
}

export interface NativeStreamHandlers {
  onFirstChunk?: (ttftMs: number) => Promise<void> | void;
  onTextDelta?: (text: string) => Promise<void> | void;
  onStreamEvent?: (event: ResponseStreamEvent) => Promise<void> | void;
}

export type TraceParent = Span | string;

export type ToolCallExecutor = (
  toolCall: PendingToolCall,
) => Promise<EventData[]>;

export interface NativeTraceOptions {
  parent?: TraceParent;
  roundIndex?: number;
}

export interface ResponsesRuntimeLike {
  runTurn(
    context: TurnContext,
    run: (turnParent: TraceParent) => Promise<string | null>,
  ): Promise<void>;
  complete(
    request: NativeResponsesRequest,
    options?: NativeTraceOptions,
  ): Promise<Response>;
  completeStream(
    request: NativeResponsesRequest,
    handlers?: NativeStreamHandlers,
    options?: NativeTraceOptions,
  ): Promise<Response>;
  traceToolCall(
    turnParent: TraceParent,
    context: TurnContext,
    toolCall: PendingToolCall,
    roundIndex: number,
    execute?: ToolCallExecutor,
  ): Promise<EventData[]>;
}

interface NativeLlmTraceOptions extends NativeTraceOptions {
  streamed: boolean;
  handlers?: NativeStreamHandlers;
}

export class ResponsesRuntime implements ResponsesRuntimeLike {
  private readonly client: OpenAI;
  private readonly costUsagePath: string[] | null;

  constructor(options: ResponsesRuntimeOptions = {}) {
    ensureBraintrustLogger(options.braintrust ?? null);
    this.costUsagePath = options.costUsagePath ?? null;
    // wrapOpenAI auto-instruments chat.completions/responses calls with a
    // braintrust LLM span. Also covers the OpenRouter path (same OpenAI client,
    // just a different base URL) — braintrust's wrapOpenRouter is for their
    // native SDK, not the OpenAI SDK, so it doesn't apply here.
    this.client = wrapOpenAI(
      new OpenAI({
        apiKey: options.apiKey ?? undefined,
        baseURL: options.baseURL,
        organization: options.organization,
        project: options.project,
        defaultHeaders: options.defaultHeaders,
      }),
    );
  }

  static fromEnvironment(agentConfig?: AgentConfig): ResponsesRuntime {
    return new ResponsesRuntime({
      apiKey: process.env.OPENAI_API_KEY,
      baseURL: process.env.OPENAI_BASE_URL,
      organization: process.env.OPENAI_ORG_ID,
      project: process.env.OPENAI_PROJECT,
      braintrust: braintrustOptionsFromAgentConfig(agentConfig),
    });
  }

  static fromModelBinding(
    agentConfig: AgentConfig | undefined,
    binding: ResponsesModelBinding,
  ): ResponsesRuntime {
    const credentials = openAiCredentials(binding);
    return new ResponsesRuntime({
      apiKey: credentials.apiKey,
      baseURL: binding.baseUrl ?? undefined,
      organization: process.env.OPENAI_ORG_ID,
      project: process.env.OPENAI_PROJECT,
      defaultHeaders: credentials.defaultHeaders,
      costUsagePath: binding.costUsagePath ?? null,
      braintrust: braintrustOptionsFromAgentConfig(agentConfig),
    });
  }

  async runTurn(
    context: TurnContext,
    run: (turnParent: TraceParent) => Promise<string | null>,
  ): Promise<void> {
    await traceExecutorTurn(context, run);
  }

  async complete(
    request: NativeResponsesRequest,
    options: NativeTraceOptions = {},
  ): Promise<Response> {
    return this.runLlmRequest(request, {
      ...options,
      streamed: false,
    });
  }

  async completeStream(
    request: NativeResponsesRequest,
    handlers: NativeStreamHandlers = {},
    options: NativeTraceOptions = {},
  ): Promise<Response> {
    return this.runLlmRequest(request, {
      ...options,
      streamed: true,
      handlers,
    });
  }

  async traceToolCall(
    turnParent: TraceParent,
    context: TurnContext,
    toolCall: PendingToolCall,
    roundIndex: number,
    execute: ToolCallExecutor = (toolCall) =>
      context.executePendingTools([toolCall]),
  ): Promise<EventData[]> {
    return tracedUnderParent(
      turnParent,
      async (span) => {
        try {
          const events = await execute(toolCall);
          span.log({ output: toolResultTraceOutput(events) });
          return events;
        } catch (error) {
          span.log({ error: errorMessage(error) });
          throw error;
        }
      },
      {
        name: toolCall.request.functionName,
        type: "tool",
        spanAttributes: { purpose: "tool_call" },
        event: {
          input: toolCall.request,
          metadata: {
            round_index: roundIndex,
          },
        },
      },
    );
  }

  private async runLlmRequest(
    request: NativeResponsesRequest,
    options: NativeLlmTraceOptions,
  ): Promise<Response> {
    const response = options.streamed
      ? await this.completeStreamRaw(
          buildStreamingBody(request),
          options.handlers,
        )
      : await this.completeRaw(buildNonStreamingBody(request));
    return this.applyProviderCost(response);
  }

  // The provider-reported spend rides on the raw usage object at the path the
  // provider record declares; surface it for usageRecord.
  private applyProviderCost(response: Response): Response {
    if (!this.costUsagePath?.length || !response.usage) {
      return response;
    }
    const cost = walkUsagePath(response.usage, this.costUsagePath);
    if (cost == null) {
      return response;
    }
    return {
      ...response,
      usage: { ...response.usage, provider_cost_usd: cost },
    } as Response;
  }

  private async completeRaw(
    body: ResponseCreateParamsNonStreaming,
  ): Promise<Response> {
    return this.client.responses.create(body);
  }

  private async completeStreamRaw(
    body: ResponseCreateParamsStreaming,
    handlers: NativeStreamHandlers = {},
  ): Promise<Response> {
    const startedAt = performance.now();
    let sawFirstChunk = false;
    let ttftMs: number | null = null;
    let finalResponse: Response | null = null;
    const stream = await this.client.responses.create(body);

    for await (const event of stream) {
      if (!sawFirstChunk) {
        sawFirstChunk = true;
        ttftMs = Math.max(0, Math.round(performance.now() - startedAt));
        await handlers.onFirstChunk?.(ttftMs);
      }

      await handlers.onStreamEvent?.(event);
      if (event.type === "response.output_text.delta") {
        await handlers.onTextDelta?.(event.delta);
      } else if (event.type === "response.completed") {
        finalResponse = event.response;
      } else if (event.type === "response.failed") {
        throw new Error(
          event.response.error?.message ?? "Responses API response failed",
        );
      }
    }

    if (!finalResponse) {
      throw new Error("Responses API stream ended without completion");
    }
    return finalResponse;
  }
}

export function runtimeFromModelBinding(
  agentConfig: AgentConfig | undefined,
  binding: ResponsesModelBinding,
): ResponsesRuntimeLike {
  // A declared wire format (from a registered provider binding) is
  // authoritative, mirroring the Rust runtime.
  switch (binding.format) {
    case "chat-completions":
      return ChatCompletionsRuntime.fromModelBinding(agentConfig, binding);
    case "responses":
      return ResponsesRuntime.fromModelBinding(agentConfig, binding);
    case "anthropic":
      return AnthropicRuntime.fromModelBinding(agentConfig, binding);
    default:
      break;
  }
  const model = binding.model ?? "";
  if (isAnthropicModel(model)) {
    return AnthropicRuntime.fromModelBinding(agentConfig, binding);
  }
  // OpenRouter is OpenAI-compatible but Chat Completions only (no Responses
  // API), so force the chat path regardless of how the model name looks.
  if (isOpenRouterBinding(binding)) {
    return ChatCompletionsRuntime.fromModelBinding(agentConfig, binding);
  }
  return modelRequiresResponsesApi(model)
    ? ResponsesRuntime.fromModelBinding(agentConfig, binding)
    : ChatCompletionsRuntime.fromModelBinding(agentConfig, binding);
}

// Anthropic model bindings call the native Messages API. We detect them by
// model name (`claude*`), mirroring the Rust runtime; Bedrock/Vertex Anthropic
// ids carry provider prefixes and intentionally don't match here.
export function isAnthropicModel(model: string): boolean {
  return model.toLowerCase().startsWith("claude");
}

// OpenRouter is selected by its base URL (it aggregates many vendors, so the
// model name isn't a reliable signal), mirroring the Rust runtime.
export function isOpenRouterBinding(binding: ResponsesModelBinding): boolean {
  return (binding.baseUrl ?? "").includes("openrouter.ai");
}

export function modelRequiresResponsesApi(model: string): boolean {
  const lower = model.toLowerCase();
  const gpt5Minor = lower.match(/^gpt-5\.(\d+)/)?.[1]?.match(/^\d+$/)?.[0];
  return (
    lower.startsWith("o1-pro") ||
    lower.startsWith("o3-pro") ||
    lower.startsWith("gpt-5-pro") ||
    (gpt5Minor !== undefined && Number(gpt5Minor) >= 3) ||
    (lower.startsWith("gpt-5") && lower.includes("-codex"))
  );
}

export class ChatCompletionsRuntime implements ResponsesRuntimeLike {
  private readonly client: OpenAI;
  private readonly costUsagePath: string[] | null;

  constructor(options: ResponsesRuntimeOptions = {}) {
    ensureBraintrustLogger(options.braintrust ?? null);
    this.costUsagePath = options.costUsagePath ?? null;
    // wrapOpenAI auto-instruments chat.completions/responses calls with a
    // braintrust LLM span. Also covers the OpenRouter path (same OpenAI client,
    // just a different base URL) — braintrust's wrapOpenRouter is for their
    // native SDK, not the OpenAI SDK, so it doesn't apply here.
    this.client = wrapOpenAI(
      new OpenAI({
        apiKey: options.apiKey ?? undefined,
        baseURL: options.baseURL,
        organization: options.organization,
        project: options.project,
        defaultHeaders: options.defaultHeaders,
      }),
    );
  }

  static fromModelBinding(
    agentConfig: AgentConfig | undefined,
    binding: ResponsesModelBinding,
  ): ChatCompletionsRuntime {
    const credentials = openAiCredentials(binding);
    return new ChatCompletionsRuntime({
      apiKey: credentials.apiKey,
      baseURL: binding.baseUrl ?? undefined,
      organization: process.env.OPENAI_ORG_ID,
      project: process.env.OPENAI_PROJECT,
      defaultHeaders: credentials.defaultHeaders,
      costUsagePath: binding.costUsagePath ?? null,
      braintrust: braintrustOptionsFromAgentConfig(agentConfig),
    });
  }

  async runTurn(
    context: TurnContext,
    run: (turnParent: TraceParent) => Promise<string | null>,
  ): Promise<void> {
    await traceExecutorTurn(context, run);
  }

  async complete(
    request: NativeResponsesRequest,
    options: NativeTraceOptions = {},
  ): Promise<Response> {
    return this.runLlmRequest(request, {
      ...options,
      streamed: false,
    });
  }

  async completeStream(
    request: NativeResponsesRequest,
    handlers: NativeStreamHandlers = {},
    options: NativeTraceOptions = {},
  ): Promise<Response> {
    return this.runLlmRequest(request, {
      ...options,
      streamed: true,
      handlers,
    });
  }

  async traceToolCall(
    turnParent: TraceParent,
    context: TurnContext,
    toolCall: PendingToolCall,
    roundIndex: number,
    execute: ToolCallExecutor = (toolCall) =>
      context.executePendingTools([toolCall]),
  ): Promise<EventData[]> {
    return tracedUnderParent(
      turnParent,
      async (span) => {
        try {
          const events = await execute(toolCall);
          span.log({ output: toolResultTraceOutput(events) });
          return events;
        } catch (error) {
          span.log({ error: errorMessage(error) });
          throw error;
        }
      },
      {
        name: toolCall.request.functionName,
        type: "tool",
        spanAttributes: { purpose: "tool_call" },
        event: {
          input: toolCall.request,
          metadata: {
            round_index: roundIndex,
          },
        },
      },
    );
  }

  private async runLlmRequest(
    request: NativeResponsesRequest,
    options: NativeLlmTraceOptions,
  ): Promise<Response> {
    if (options.streamed) {
      return this.completeStreamRaw(
        buildChatStreamingBody(request),
        options.handlers,
      );
    }
    return chatCompletionToResponse(
      await this.completeRaw(buildChatNonStreamingBody(request)),
      this.costUsagePath,
    );
  }

  private async completeRaw(
    body: ChatCompletionCreateParamsNonStreaming,
  ): Promise<ChatCompletion> {
    return this.client.chat.completions.create(body);
  }

  private async completeStreamRaw(
    body: ChatCompletionCreateParamsStreaming,
    handlers: NativeStreamHandlers = {},
  ): Promise<Response> {
    const startedAt = performance.now();
    let sawFirstChunk = false;
    let ttftMs: number | null = null;
    const accumulator = new ChatCompletionAccumulator(this.costUsagePath);
    const stream = await this.client.chat.completions.create(body);

    for await (const chunk of stream) {
      if (!sawFirstChunk) {
        sawFirstChunk = true;
        ttftMs = Math.max(0, Math.round(performance.now() - startedAt));
        await handlers.onFirstChunk?.(ttftMs);
      }
      accumulator.push(chunk);
      const text = chunk.choices[0]?.delta.content;
      if (text) {
        await handlers.onTextDelta?.(text);
      }
    }

    return accumulator.finalize();
  }
}

// Anthropic's Messages API requires `max_tokens`; the OpenAI side leaves it
// optional. Use the binding's configured limit when present, otherwise a
// conservative default.
const DEFAULT_ANTHROPIC_MAX_TOKENS = 4096;

// Mirrors ChatCompletionsRuntime: build a provider-native request, call the
// provider SDK, then normalize the provider response into the OpenAI Responses
// `Response` shape that the rest of the harness consumes.
export class AnthropicRuntime implements ResponsesRuntimeLike {
  private readonly client: Anthropic;
  private readonly costUsagePath: string[] | null;

  constructor(options: ResponsesRuntimeOptions = {}) {
    ensureBraintrustLogger(options.braintrust ?? null);
    this.costUsagePath = options.costUsagePath ?? null;
    // wrapAnthropic auto-instruments every messages.create/.stream call with a
    // braintrust LLM span (input/output/usage), so we don't hand-roll spans.
    this.client = wrapAnthropic(
      new Anthropic({
        apiKey: options.apiKey,
        authToken: options.authToken,
        baseURL: options.baseURL,
        defaultHeaders: options.defaultHeaders,
      }),
    );
  }

  static fromModelBinding(
    agentConfig: AgentConfig | undefined,
    binding: ResponsesModelBinding,
  ): AnthropicRuntime {
    const baseURL = binding.baseUrl ?? undefined;
    const costUsagePath = binding.costUsagePath ?? null;
    if (binding.auth === "none") {
      // The SDK refuses to construct without a credential unless the auth
      // headers are explicitly omitted via null default headers.
      return new AnthropicRuntime({
        apiKey: null,
        authToken: null,
        defaultHeaders: { "x-api-key": null, authorization: null },
        baseURL,
        costUsagePath,
        braintrust: braintrustOptionsFromAgentConfig(agentConfig),
      });
    }
    if (isProviderDeclared(binding) && !binding.apiKey) {
      // Mirror the Rust runtime: never silently unauthenticated.
      throw new Error(
        "model request is missing an API key (the provider has no secret)",
      );
    }
    if (binding.auth === "bearer") {
      // Anthropic wire format with bearer auth (e.g. gateways like Opper):
      // the SDK's authToken sends `Authorization: Bearer ...`; null apiKey
      // disables both the x-api-key header and the env fallbacks.
      return new AnthropicRuntime({
        apiKey: null,
        authToken: binding.apiKey,
        baseURL,
        costUsagePath,
        braintrust: braintrustOptionsFromAgentConfig(agentConfig),
      });
    }
    if (isProviderDeclared(binding)) {
      // Native x-api-key: null authToken blocks the ANTHROPIC_AUTH_TOKEN env
      // fallback so only the provider's own credential is ever sent.
      return new AnthropicRuntime({
        apiKey: binding.apiKey,
        authToken: null,
        baseURL,
        costUsagePath,
        braintrust: braintrustOptionsFromAgentConfig(agentConfig),
      });
    }
    return new AnthropicRuntime({
      apiKey: binding.apiKey,
      baseURL,
      braintrust: braintrustOptionsFromAgentConfig(agentConfig),
    });
  }

  async runTurn(
    context: TurnContext,
    run: (turnParent: TraceParent) => Promise<string | null>,
  ): Promise<void> {
    await traceExecutorTurn(context, run);
  }

  async complete(
    request: NativeResponsesRequest,
    options: NativeTraceOptions = {},
  ): Promise<Response> {
    return this.runLlmRequest(request, {
      ...options,
      streamed: false,
    });
  }

  async completeStream(
    request: NativeResponsesRequest,
    handlers: NativeStreamHandlers = {},
    options: NativeTraceOptions = {},
  ): Promise<Response> {
    return this.runLlmRequest(request, {
      ...options,
      streamed: true,
      handlers,
    });
  }

  async traceToolCall(
    turnParent: TraceParent,
    context: TurnContext,
    toolCall: PendingToolCall,
    roundIndex: number,
    execute: ToolCallExecutor = (toolCall) =>
      context.executePendingTools([toolCall]),
  ): Promise<EventData[]> {
    return tracedUnderParent(
      turnParent,
      async (span) => {
        try {
          const events = await execute(toolCall);
          span.log({ output: toolResultTraceOutput(events) });
          return events;
        } catch (error) {
          span.log({ error: errorMessage(error) });
          throw error;
        }
      },
      {
        name: toolCall.request.functionName,
        type: "tool",
        spanAttributes: { purpose: "tool_call" },
        event: {
          input: toolCall.request,
          metadata: {
            round_index: roundIndex,
          },
        },
      },
    );
  }

  private async runLlmRequest(
    request: NativeResponsesRequest,
    options: NativeLlmTraceOptions,
  ): Promise<Response> {
    const body = buildAnthropicBody(request);
    if (options.streamed) {
      return this.completeStreamRaw(body, options.handlers);
    }
    return anthropicMessageToResponse(
      await this.client.messages.create(body),
      this.costUsagePath,
    );
  }

  private async completeStreamRaw(
    body: Anthropic.MessageCreateParamsNonStreaming,
    handlers: NativeStreamHandlers = {},
  ): Promise<Response> {
    const startedAt = performance.now();
    let sawFirstChunk = false;
    let ttftMs: number | null = null;
    const stream = this.client.messages.stream(body);

    let providerCost: number | null = null;
    for await (const event of stream) {
      if (this.costUsagePath?.length) {
        providerCost =
          costFromAnthropicStreamEvent(event, this.costUsagePath) ??
          providerCost;
      }
      if (
        event.type === "content_block_delta" &&
        event.delta.type === "text_delta"
      ) {
        if (!sawFirstChunk) {
          sawFirstChunk = true;
          ttftMs = Math.max(0, Math.round(performance.now() - startedAt));
          await handlers.onFirstChunk?.(ttftMs);
        }
        await handlers.onTextDelta?.(event.delta.text);
      }
    }

    const response = anthropicMessageToResponse(
      await stream.finalMessage(),
      this.costUsagePath,
    );
    if (providerCost != null && response.usage) {
      return {
        ...response,
        usage: { ...response.usage, provider_cost_usd: providerCost },
      } as Response;
    }
    return response;
  }
}

function buildAnthropicBody(
  request: NativeResponsesRequest,
): Anthropic.MessageCreateParamsNonStreaming {
  const { system, messages } = splitAnthropicMessages(request.messages ?? []);
  const tools = toolDefinitionsToAnthropicTools(request.tools ?? []);
  return {
    model: request.model,
    max_tokens: request.maxOutputTokens ?? DEFAULT_ANTHROPIC_MAX_TOKENS,
    system: system.length === 0 ? undefined : system,
    messages,
    tools: tools.length === 0 ? undefined : tools,
  };
}

// Anthropic takes the system prompt as a top-level field, not a message role.
// Pull system/developer turns out, then let lingua convert the rest — the same
// `linguaTo<Provider>Messages` path the Responses runtime uses for its input.
function splitAnthropicMessages(messages: Message[]): {
  system: string;
  messages: Anthropic.MessageParam[];
} {
  const systemParts: string[] = [];
  const conversation: Message[] = [];
  for (const message of messages) {
    if (message.role === "system" || message.role === "developer") {
      systemParts.push(messageContentText(message.content));
    } else {
      conversation.push(message);
    }
  }
  return {
    system: systemParts.join("\n\n"),
    messages: linguaToAnthropicMessages(
      conversation as LinguaMessage[],
    ) as Anthropic.MessageParam[],
  };
}

function toolDefinitionsToAnthropicTools(
  tools: ToolDefinition[],
): Anthropic.Tool[] {
  return tools.map((tool) => ({
    name: tool.name,
    description: tool.description,
    input_schema: tool.parameters as Anthropic.Tool.InputSchema,
  }));
}

function anthropicMessageToResponse(
  message: Anthropic.Message,
  costUsagePath?: string[] | null,
): Response {
  const output: unknown[] = [];
  const text = message.content
    .filter((block): block is Anthropic.TextBlock => block.type === "text")
    .map((block) => block.text)
    .join("");
  if (text.length > 0) {
    output.push(responseMessageOutput(`${message.id}_message`, text));
  }
  for (const block of message.content) {
    if (block.type === "tool_use") {
      output.push(
        responseFunctionCallOutput({
          id: block.id,
          type: "function",
          function: {
            name: block.name,
            arguments: JSON.stringify(block.input ?? {}),
          },
        } as ChatFunctionToolCall),
      );
    }
  }
  return {
    id: message.id,
    object: "response",
    created_at: Math.floor(Date.now() / 1000),
    status: "completed",
    model: message.model,
    output,
    usage: anthropicUsageToResponseUsage(message.usage, costUsagePath),
  } as unknown as Response;
}

function anthropicUsageToResponseUsage(
  usage: Anthropic.Usage | null | undefined,
  costUsagePath?: string[] | null,
): unknown {
  if (!usage) {
    return null;
  }
  const input = usage.input_tokens ?? 0;
  const output = usage.output_tokens ?? 0;
  const cached = usage.cache_read_input_tokens ?? 0;
  const providerCost = costUsagePath?.length
    ? walkUsagePath(usage, costUsagePath)
    : null;
  return {
    input_tokens: input,
    output_tokens: output,
    total_tokens: input + output,
    input_tokens_details: { cached_tokens: cached },
    output_tokens_details: { reasoning_tokens: 0 },
    ...(providerCost != null ? { provider_cost_usd: providerCost } : {}),
  };
}

export async function runResponsesTurn(
  context: TurnContext,
  run: (
    runtime: ResponsesRuntimeLike,
    context: TurnContext,
    turnParent: TraceParent,
  ) => Promise<string | null>,
): Promise<void> {
  const runtime = ResponsesRuntime.fromEnvironment(context.agentConfig);
  await runtime.runTurn(context, (turnParent) =>
    run(runtime, context, turnParent),
  );
}

export async function traceExecutorTurn(
  context: TurnContext,
  run: (turnParent: TraceParent) => Promise<string | null>,
): Promise<void> {
  ensureBraintrustLogger(braintrustOptionsFromAgentConfig(context.agentConfig));
  try {
    if (context.braintrustParent) {
      await run(context.braintrustParent);
    } else {
      await traceRootExecutorTurn(context, run);
    }
  } finally {
    await flushNativeBraintrust();
  }
}

async function traceRootExecutorTurn(
  context: TurnContext,
  run: (turnParent: Span) => Promise<string | null>,
): Promise<void> {
  const { agent, conversation, turn } = context.exoharness.current;
  await traced(
    (sessionSpan) =>
      sessionSpan.traced(
        async (turnSpan) => {
          try {
            const latestEventId = await run(turnSpan);
            turnSpan.log({
              metadata: {
                status: "ok",
                latest_event_id: latestEventId,
              },
            });
          } catch (error) {
            turnSpan.log({
              error: errorMessage(error),
              metadata: { status: "error" },
            });
            throw error;
          }
        },
        {
          name: "executor_turn",
          type: "task",
          spanAttributes: { purpose: "executor_turn" },
          event: {
            metadata: {
              session_id: turn.record.sessionId,
              turn_id: turn.record.id,
              model: context.agentConfig.model,
              streamed: context.streaming,
            },
          },
        },
      ),
    {
      name: "executor_session",
      type: "task",
      spanAttributes: { purpose: "executor_session" },
      event: {
        metadata: {
          agent_id: agent.record.id,
          agent_slug: agent.record.slug,
          conversation_id: conversation.record.id,
          conversation_slug: conversation.record.slug,
          session_id: turn.record.sessionId,
          model: context.agentConfig.model,
        },
      },
    },
  );
}

function buildNonStreamingBody(
  request: NativeResponsesRequest,
): ResponseCreateParamsNonStreaming {
  return {
    model: request.model as ResponseCreateParamsNonStreaming["model"],
    input: request.input ?? linguaMessagesToResponsesInput(request.messages),
    tools:
      request.responseTools ??
      toolDefinitionsToResponsesTools(request.tools ?? []),
    max_output_tokens: request.maxOutputTokens ?? null,
    metadata: request.metadata ?? null,
    stream: false,
    store: false,
  };
}

function buildStreamingBody(
  request: NativeResponsesRequest,
): ResponseCreateParamsStreaming {
  return {
    model: request.model as ResponseCreateParamsStreaming["model"],
    input: request.input ?? linguaMessagesToResponsesInput(request.messages),
    tools:
      request.responseTools ??
      toolDefinitionsToResponsesTools(request.tools ?? []),
    max_output_tokens: request.maxOutputTokens ?? null,
    metadata: request.metadata ?? null,
    stream: true,
    store: false,
  };
}

function buildChatNonStreamingBody(
  request: NativeResponsesRequest,
): ChatCompletionCreateParamsNonStreaming {
  const tools = toolDefinitionsToChatTools(request.tools ?? []);
  return {
    model: request.model,
    messages: messagesToChatMessages(request.messages ?? []),
    tools: tools.length === 0 ? undefined : tools,
    tool_choice: tools.length === 0 ? undefined : "auto",
    max_tokens: request.maxOutputTokens ?? undefined,
    stream: false,
  };
}

function buildChatStreamingBody(
  request: NativeResponsesRequest,
): ChatCompletionCreateParamsStreaming {
  const tools = toolDefinitionsToChatTools(request.tools ?? []);
  return {
    model: request.model,
    messages: messagesToChatMessages(request.messages ?? []),
    tools: tools.length === 0 ? undefined : tools,
    tool_choice: tools.length === 0 ? undefined : "auto",
    max_tokens: request.maxOutputTokens ?? undefined,
    stream: true,
    stream_options: { include_usage: true },
  };
}

function messagesToChatMessages(
  messages: Message[],
): ChatCompletionMessageParam[] {
  return messages.map(messageToChatMessage);
}

function messageToChatMessage(message: Message): ChatCompletionMessageParam {
  if (message.role === "system" || message.role === "developer") {
    return { role: "system", content: messageContentText(message.content) };
  }
  if (message.role === "user") {
    return { role: "user", content: messageContentText(message.content) };
  }
  if (message.role === "tool") {
    const result = toolResultContent(message.content);
    return {
      role: "tool",
      tool_call_id: result.toolCallId,
      content: JSON.stringify(result.output),
    };
  }
  const toolCalls = assistantToolCalls(message.content);
  return {
    role: "assistant",
    content: assistantTextContent(message.content),
    tool_calls: toolCalls.length === 0 ? undefined : toolCalls,
  };
}

function toolDefinitionsToChatTools(
  tools: ToolDefinition[],
): ChatCompletionTool[] {
  return tools.map((tool) => ({
    type: "function",
    function: {
      name: tool.name,
      description: tool.description,
      parameters: tool.parameters as JsonObject,
      strict: true,
    },
  }));
}

function chatCompletionToResponse(
  completion: ChatCompletion,
  costUsagePath?: string[] | null,
): Response {
  const choice = completion.choices[0];
  const output: unknown[] = [];
  if (choice?.message.content) {
    output.push(
      responseMessageOutput(`${completion.id}_message`, choice.message.content),
    );
  }
  for (const toolCall of choice?.message.tool_calls ?? []) {
    if (toolCall.type === "function") {
      output.push(responseFunctionCallOutput(toolCall));
    }
  }
  return {
    id: completion.id,
    object: "response",
    created_at: completion.created,
    status: "completed",
    model: completion.model,
    output,
    usage: chatUsageToResponseUsage(completion.usage, costUsagePath),
  } as unknown as Response;
}

class ChatCompletionAccumulator {
  constructor(private readonly costUsagePath: string[] | null = null) {}
  private id = `chatcmpl_${Date.now()}`;
  private created = Math.floor(Date.now() / 1000);
  private model = "";
  private content = "";
  private usage: ChatCompletionChunk["usage"] | null = null;
  private readonly toolCalls = new Map<
    number,
    {
      id?: string;
      name?: string;
      arguments: string;
    }
  >();

  push(chunk: ChatCompletionChunk): void {
    this.id = chunk.id || this.id;
    this.created = chunk.created || this.created;
    this.model = chunk.model || this.model;
    this.usage = chunk.usage ?? this.usage;
    for (const choice of chunk.choices) {
      const delta = choice.delta;
      if (delta.content) {
        this.content += delta.content;
      }
      for (const toolCall of delta.tool_calls ?? []) {
        const index = toolCall.index;
        const current = this.toolCalls.get(index) ?? { arguments: "" };
        current.id = toolCall.id ?? current.id;
        current.name = toolCall.function?.name ?? current.name;
        current.arguments += toolCall.function?.arguments ?? "";
        this.toolCalls.set(index, current);
      }
    }
  }

  finalize(): Response {
    const output: unknown[] = [];
    if (this.content.length > 0) {
      output.push(responseMessageOutput(`${this.id}_message`, this.content));
    }
    for (const [, toolCall] of [...this.toolCalls.entries()].sort(
      ([left], [right]) => left - right,
    )) {
      if (!toolCall.id || !toolCall.name) {
        continue;
      }
      output.push(
        responseFunctionCallOutput({
          id: toolCall.id,
          type: "function",
          function: {
            name: toolCall.name,
            arguments: toolCall.arguments,
          },
        } as ChatFunctionToolCall),
      );
    }
    return {
      id: this.id,
      object: "response",
      created_at: this.created,
      status: "completed",
      model: this.model,
      output,
      usage: chatUsageToResponseUsage(this.usage, this.costUsagePath),
    } as unknown as Response;
  }
}

function responseMessageOutput(id: string, text: string): unknown {
  return {
    id,
    type: "message",
    role: "assistant",
    status: "completed",
    content: [
      {
        type: "output_text",
        text,
        annotations: [],
      },
    ],
  };
}

type ChatFunctionToolCall = Extract<
  ChatCompletionMessageToolCall,
  { type: "function" }
>;

function responseFunctionCallOutput(toolCall: ChatFunctionToolCall): unknown {
  return {
    id: `${toolCall.id}_item`,
    type: "function_call",
    call_id: toolCall.id,
    name: toolCall.function.name,
    arguments: toolCall.function.arguments,
    status: "completed",
  };
}

function chatUsageToResponseUsage(
  usage:
    | ChatCompletion["usage"]
    | ChatCompletionChunk["usage"]
    | null
    | undefined,
  costUsagePath?: string[] | null,
): unknown {
  if (!usage) {
    return null;
  }
  const providerCost = costUsagePath?.length
    ? walkUsagePath(usage, costUsagePath)
    : null;
  return {
    ...(providerCost != null ? { provider_cost_usd: providerCost } : {}),
    input_tokens: usage.prompt_tokens,
    output_tokens: usage.completion_tokens,
    total_tokens: usage.total_tokens,
    input_tokens_details: {
      cached_tokens: usage.prompt_tokens_details?.cached_tokens ?? 0,
    },
    output_tokens_details: {
      reasoning_tokens: usage.completion_tokens_details?.reasoning_tokens ?? 0,
    },
  };
}

function assistantToolCalls(content: unknown): ChatCompletionMessageToolCall[] {
  if (!Array.isArray(content)) {
    return [];
  }
  return content.flatMap((part): ChatCompletionMessageToolCall[] => {
    if (!isRecord(part) || part.type !== "tool_call") {
      return [];
    }
    if (
      typeof part.tool_call_id !== "string" ||
      typeof part.tool_name !== "string"
    ) {
      return [];
    }
    return [
      {
        id: part.tool_call_id,
        type: "function",
        function: {
          name: part.tool_name,
          arguments: JSON.stringify(
            isRecord(part.arguments) ? part.arguments : {},
          ),
        },
      },
    ];
  });
}

function assistantTextContent(content: unknown): string | null {
  if (Array.isArray(content)) {
    const text = content
      .filter((part) => isRecord(part) && part.type === "text")
      .map((part) => messageContentText((part as { text?: unknown }).text))
      .join("");
    return text || null;
  }
  return messageContentText(content);
}

function toolResultContent(content: unknown): {
  toolCallId: string;
  output: unknown;
} {
  const part = Array.isArray(content) ? content.find(isRecord) : null;
  if (
    !isRecord(part) ||
    part.type !== "tool_result" ||
    typeof part.tool_call_id !== "string"
  ) {
    throw new Error("tool message must contain a tool_result content part");
  }
  return {
    toolCallId: part.tool_call_id,
    output: part.output,
  };
}

function messageContentText(content: unknown): string {
  if (typeof content === "string") {
    return content;
  }
  if (content === null || content === undefined) {
    return "";
  }
  return JSON.stringify(content);
}

export function tracedUnderParent<R>(
  parent: TraceParent | undefined,
  run: (span: Span) => R,
  args: StartSpanArgs,
): R {
  if (!parent) {
    return traced(run, args);
  }
  if (typeof parent === "string") {
    return traced(run, { ...args, parent });
  }
  return parent.traced(run, args);
}

export function linguaMessagesToResponsesInput(
  messages: Message[] | undefined,
): ResponseInput {
  const items = linguaToResponsesMessages<ResponseInput>(
    (messages ?? []) as LinguaMessage[],
  );
  // Requests are sent with `store: false`, so server-side item ids from prior
  // rounds (rs_/fc_/msg_) don't resolve — replaying them 404s on reasoning
  // models. Replay statelessly: drop reasoning items (lingua doesn't preserve
  // encrypted_content, so a bare id is all we'd have) and strip item ids.
  return items.flatMap((item) => {
    if (!isRecord(item)) return [item];
    if (item.type === "reasoning") return [];
    if (typeof item.id === "string") {
      const { id: _id, ...rest } = item;
      return [rest as typeof item];
    }
    return [item];
  }) as ResponseInput;
}

export function responseToLinguaEvents(response: Response): EventData[] {
  const events: EventData[] = [];
  const messages = responseMessages(response);
  if (messages.length > 0) {
    events.push(messagesEvent(messages, undefined, usageRecord(response)));
  }
  for (const result of responseToolCallResults(response)) {
    if (result.type === "tool_call") {
      events.push(toolRequestedEvent(result.toolCall));
    } else {
      events.push(
        toolResultEvent(result.toolCallId, {
          ok: false,
          error: result.error,
        }),
      );
    }
  }
  return events;
}

// Policy: attach raw usage + cost to the messages event. cost_usd is filled from
// the shared price cache; left unset if the cache is unavailable.
function usageRecord(response: Response): JsonObject | undefined {
  const usage = response.usage;
  if (!usage) return undefined;
  const prompt = usage.input_tokens;
  const completion = usage.output_tokens;
  const cached = usage.input_tokens_details?.cached_tokens;
  const reasoning = usage.output_tokens_details?.reasoning_tokens;
  const providerCost = (usage as { provider_cost_usd?: unknown })
    .provider_cost_usd;
  const table = getTable();
  const tableCost = table
    ? computeCostUsd(table, response.model, { prompt, completion, cached })
    : null;
  // The provider-reported spend is authoritative when the record declares
  // where to find it; the price table is the estimate fallback.
  const cost = typeof providerCost === "number" ? providerCost : tableCost;

  const record: JsonObject = { model: response.model };
  if (prompt != null) record.prompt_tokens = prompt;
  if (completion != null) record.completion_tokens = completion;
  if (cached != null) record.prompt_cached_tokens = cached;
  if (reasoning != null) record.completion_reasoning_tokens = reasoning;
  if (cost != null) record.cost_usd = cost;
  return record;
}

export function responseStreamEventToLinguaEvents(
  event: ResponseStreamEvent,
): EventData[] {
  return event.type === "response.completed"
    ? responseToLinguaEvents(event.response)
    : [];
}

export function responseMessages(response: Response): Message[] {
  return responsesMessagesToLingua(response.output) as Message[];
}

export function responseToolCalls(response: Response): PendingToolCall[] {
  return responseToolCallResults(response).flatMap((result) =>
    result.type === "tool_call" ? [result.toolCall] : [],
  );
}

type ResponseToolCallResult =
  | {
      type: "tool_call";
      toolCall: PendingToolCall;
    }
  | {
      type: "parse_error";
      toolCallId: string;
      error: string;
    };

function responseToolCallResults(response: Response): ResponseToolCallResult[] {
  return response.output
    .filter((item) => item.type === "function_call")
    .map((item) => {
      const parsed = parseJsonObject(item.arguments);
      if (!parsed.ok) {
        return {
          type: "parse_error",
          toolCallId: item.call_id,
          error: `Invalid JSON arguments for ${item.name}: ${parsed.error}`,
        };
      }
      return {
        type: "tool_call",
        toolCall: {
          toolCallId: item.call_id,
          request: {
            functionName: item.name,
            arguments: parsed.value,
          },
        },
      };
    });
}

export function toolDefinitionsToResponsesTools(
  tools: ToolDefinition[],
): Tool[] {
  return tools.map((tool) => ({
    type: "function",
    name: tool.name,
    description: tool.description,
    parameters: tool.parameters as JsonObject,
    strict: true,
  }));
}

export async function flushNativeBraintrust(): Promise<void> {
  await flush();
}

let initializedBraintrustKey: string | null = null;

function ensureBraintrustLogger(options: NativeBraintrustOptions | null): void {
  const apiKey = options?.apiKey ?? process.env.BRAINTRUST_API_KEY;
  if (!apiKey) {
    return;
  }

  const loggerOptions = {
    apiKey,
    appUrl: options?.appUrl ?? process.env.BRAINTRUST_APP_URL,
    orgName: options?.orgName,
    projectName: options?.projectName,
    projectId: options?.projectId,
    asyncFlush: true,
  };
  const key = JSON.stringify(loggerOptions);
  if (initializedBraintrustKey === key) {
    return;
  }
  initLogger(loggerOptions);
  initializedBraintrustKey = key;
}

function braintrustOptionsFromAgentConfig(
  agentConfig: AgentConfig | undefined,
): NativeBraintrustOptions | null {
  const raw = agentConfig?.braintrust;
  if (!isRecord(raw)) {
    return null;
  }

  const project = raw.project;
  const options: NativeBraintrustOptions = {
    apiKey: process.env.BRAINTRUST_API_KEY,
    appUrl: process.env.BRAINTRUST_APP_URL,
    orgName: stringField(raw, "org_name") ?? stringField(raw, "orgName"),
  };

  if (isRecord(project)) {
    const kind = stringField(project, "kind");
    const value = stringField(project, "value");
    if (kind === "name") {
      options.projectName = value;
    } else if (kind === "id") {
      options.projectId = value;
    }
  }

  return options;
}

function toolResultTraceOutput(events: EventData[]): unknown {
  const results = events
    .filter((event) => event.type === "tool_result")
    .map((event) => event.result);
  return results.length === 1 ? results[0] : results;
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

type JsonObjectParseResult =
  | { ok: true; value: JsonObject }
  | { ok: false; error: string };

function parseJsonObject(json: string): JsonObjectParseResult {
  try {
    const value = JSON.parse(json) as unknown;
    if (!isRecord(value)) {
      return {
        ok: false,
        error: "function call arguments must be a JSON object",
      };
    }
    return { ok: true, value: value as JsonObject };
  } catch (error) {
    return {
      ok: false,
      error: errorMessage(error),
    };
  }
}

function stringField(
  record: Record<string, unknown>,
  key: string,
): string | undefined {
  const value = record[key];
  return typeof value === "string" ? value : undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
