import vm from "node:vm";

import {
  appendCustomEvent,
  assertRoundBudget,
  assistantMessagesText,
  assistantTextMessage,
  defineHarness,
  materializeConversationMessages,
  messagesEvent,
  messagesToHistoryMessages,
  messagesToTranscript,
  stringifyValue,
  systemTextMessage,
  toolResultMessage,
  turnMetadata,
  userTextMessage,
  type HistoryMessage,
  type JsonObject,
  type JsonValue,
  type Message,
  type PendingToolCall,
  type ToolDefinition,
  type ToolRequest,
  type ToolResult,
  type Turn,
  type TurnContext,
} from "@exo/harness";
import {
  errorMessage,
  responseMessages,
  responseToolCalls,
  ResponsesRuntime,
  tracedUnderParent,
  type ResponsesRuntimeLike,
  type TraceParent,
} from "@exo/model-runtime/responses";

import {
  resolveLlmBinding,
  type ResolvedLlmBinding,
} from "@exo/model-runtime/shared";

const STDOUT_PREVIEW_CHARS = 12_000;
const RESULT_PREVIEW_CHARS = 12_000;
const CONTEXT_PREVIEW_CHARS = 400;
const FINAL_PREVIEW_CHARS = 400;
const MAX_VARIABLE_NAMES = 128;
const REPL_TIMEOUT_MS = 2_000;

interface ReplExecutionResult {
  stdout: string;
  variable_names: string[];
  error: string | null;
  final_preview: string | null;
}

interface ReplExecuteArguments {
  code: string;
}

interface SubqueryArguments {
  prompt: string;
  target_var: string | null;
}

interface SubqueryVariableArguments {
  variable_name: string;
  question: string;
  target_var: string | null;
}

interface SubqueryToolResult {
  result: string;
  truncated: boolean;
  stored_in: string | null;
}

type FinalDirective =
  | { type: "direct"; value: string }
  | { type: "variable"; name: string };

export default defineHarness({
  async runTurn(context) {
    const modelBinding = await resolveLlmBinding(context);
    const runtime = ResponsesRuntime.fromModelBinding(
      context.agentConfig,
      modelBinding,
    );
    await runtime.runTurn(context, (turnParent) =>
      runRlmTurnLoop(runtime, context, turnParent, modelBinding),
    );
  },
});

async function runRlmTurnLoop(
  runtime: ResponsesRuntimeLike,
  context: TurnContext,
  turnParent: TraceParent,
  modelBinding: ResolvedLlmBinding,
): Promise<string | null> {
  const { conversation, turn } = context.exoharness.current;
  const contextMessages = await materializeConversationMessages(conversation);
  const contextText = messagesToTranscript(contextMessages);
  const queryText = messagesToTranscript(context.request.input);
  const repl = new JsReplState(
    contextText,
    messagesToHistoryMessages(contextMessages),
  );
  await appendCustomEvent(turn, "rlm_context_initialized", {
    engine: "node_vm",
    context_chars: [...contextText].length,
    query_chars: [...queryText].length,
  });

  const history: Message[] = [
    ...context.agentConfig.instructions,
    systemTextMessage(buildRlmSystemPrompt()),
    userTextMessage(buildRlmRootPrompt(queryText, contextText)),
  ];

  for (let round = 0; ; round += 1) {
    assertRoundBudget(context, round, "RLM turn");

    const response = await runtime.complete(
      {
        model: modelBinding.model,
        maxOutputTokens: context.agentConfig.maxOutputTokens,
        messages: history,
        tools: buildRlmToolDefinitions(),
        metadata: turnMetadata(context, {
          rlm_round: String(round),
        }),
      },
      {
        parent: turnParent,
        roundIndex: round,
      },
    );
    const modelMessages = responseMessages(response);
    const toolCalls = responseToolCalls(response);

    await appendCustomEvent(turn, "rlm_model_response", {
      round,
      response_id: response.id,
      messages: modelMessages,
      tool_call_count: toolCalls.length,
    });
    history.push(...modelMessages);

    if (toolCalls.length === 0) {
      const finalAnswer = resolveFinalAnswer(repl, modelMessages);
      return appendFinalAnswer(turn, response.id, round, finalAnswer);
    }

    const toolMessages: Message[] = [];
    for (const toolCall of toolCalls) {
      if (context.streaming) {
        await context.stream.toolCall({
          toolCallId: toolCall.toolCallId,
          toolName: toolCall.request.functionName,
          arguments: toolCall.request.arguments,
        });
      }

      await appendCustomEvent(turn, "rlm_tool_call", {
        round,
        tool_call_id: toolCall.toolCallId,
        request: {
          function_name: toolCall.request.functionName,
          arguments: toolCall.request.arguments,
        },
      });

      const result = await traceRlmToolCall(
        runtime,
        context,
        repl,
        toolCall,
        round,
        turnParent,
        modelBinding,
      );

      await appendCustomEvent(turn, "rlm_tool_result", {
        round,
        tool_call_id: toolCall.toolCallId,
        result,
      });
      if (context.streaming) {
        await context.stream.toolResult({
          toolCallId: toolCall.toolCallId,
          result,
        });
      }

      const finalValue = repl.finalValue();
      if (finalValue !== null) {
        return appendFinalAnswer(turn, response.id, round, finalValue);
      }

      toolMessages.push(
        toolResultMessage(
          toolCall.toolCallId,
          toolCall.request.functionName,
          result,
        ),
      );
    }
    history.push(...toolMessages);
  }
}

class JsReplState {
  private globals: JsonObject = {};

  constructor(
    private readonly contextText: string,
    private readonly historyMessages: HistoryMessage[],
  ) {}

  execute(code: string): ReplExecutionResult {
    const stdout: string[] = [];
    const sandbox = this.buildSandbox(stdout);
    let error: string | null = null;

    try {
      vm.runInContext(code, vm.createContext(sandbox), {
        timeout: REPL_TIMEOUT_MS,
        displayErrors: true,
      });
    } catch (caught) {
      error =
        caught instanceof Error
          ? (caught.stack ?? caught.message)
          : String(caught);
    }

    this.globals = collectJsonGlobals(sandbox);
    const final = this.globals.Final;
    return {
      stdout: clampPreview(stdout.join("\n"), STDOUT_PREVIEW_CHARS),
      variable_names: Object.keys(this.globals)
        .sort()
        .slice(0, MAX_VARIABLE_NAMES),
      error,
      final_preview:
        final === null || final === undefined
          ? null
          : clampPreview(stringifyValue(final), FINAL_PREVIEW_CHARS),
    };
  }

  readVariable(variableName: string): string {
    if (!(variableName in this.globals)) {
      throw new Error(`variable not found: ${variableName}`);
    }
    return stringifyValue(this.globals[variableName]);
  }

  setVariable(variableName: string, value: string): void {
    this.globals[variableName] = value;
  }

  finalValue(): string | null {
    if (!("Final" in this.globals)) {
      return null;
    }
    const final = this.globals.Final;
    if (final === null || final === undefined) {
      return null;
    }
    return typeof final === "string" ? final : JSON.stringify(final);
  }

  private buildSandbox(stdout: string[]): Record<string, unknown> {
    const print = (...args: unknown[]) => {
      stdout.push(args.map(renderReplValue).join(" "));
    };
    const historyMessages = this.historyMessages;
    const sandbox: Record<string, unknown> = {
      ...this.globals,
      context: this.contextText,
      getMessages(role: string | null = null): HistoryMessage[] {
        const messages =
          role === null
            ? historyMessages
            : historyMessages.filter(
                (message) => message.role === String(role).toLowerCase(),
              );
        return JSON.parse(JSON.stringify(messages)) as HistoryMessage[];
      },
      print,
      console: Object.freeze({
        log: print,
        warn: print,
        error: print,
      }),
    };
    if (!("Final" in sandbox)) {
      sandbox.Final = null;
    }
    return sandbox;
  }
}

async function traceRlmToolCall(
  runtime: ResponsesRuntimeLike,
  context: TurnContext,
  repl: JsReplState,
  toolCall: PendingToolCall,
  roundIndex: number,
  turnParent: TraceParent,
  modelBinding: ResolvedLlmBinding,
): Promise<ToolResult> {
  return tracedUnderParent(
    turnParent,
    async (span) => {
      try {
        const result = await executeRlmTool(
          runtime,
          context,
          repl,
          toolCall.request,
          turnParent,
          roundIndex,
          modelBinding,
        );
        span.log({ output: result });
        return result;
      } catch (error) {
        span.log({ error: errorMessage(error) });
        throw error;
      }
    },
    {
      name: toolCall.request.functionName,
      type: "tool",
      spanAttributes: { purpose: "rlm_tool_call" },
      event: {
        input: toolCall.request,
        metadata: {
          round_index: roundIndex,
          tool_call_id: toolCall.toolCallId,
        },
      },
    },
  );
}

async function executeRlmTool(
  runtime: ResponsesRuntimeLike,
  context: TurnContext,
  repl: JsReplState,
  request: ToolRequest,
  turnParent: TraceParent,
  roundIndex: number,
  modelBinding: ResolvedLlmBinding,
): Promise<ToolResult> {
  switch (request.functionName) {
    case "repl_execute": {
      const args = parseReplExecuteArguments(request.arguments);
      return repl.execute(args.code) as unknown as ToolResult;
    }
    case "subquery": {
      const args = parseSubqueryArguments(request.arguments);
      return runSubqueryTool(
        runtime,
        context,
        repl,
        args.prompt,
        args.target_var,
        turnParent,
        roundIndex,
        modelBinding,
      );
    }
    case "subquery_variable": {
      const args = parseSubqueryVariableArguments(request.arguments);
      const variableText = repl.readVariable(args.variable_name);
      const prompt = `${args.question}\n\nContext:\n${variableText}`;
      return runSubqueryTool(
        runtime,
        context,
        repl,
        prompt,
        args.target_var,
        turnParent,
        roundIndex,
        modelBinding,
      );
    }
    default:
      throw new Error(`unsupported RLM tool: ${request.functionName}`);
  }
}

async function runSubqueryTool(
  runtime: ResponsesRuntimeLike,
  context: TurnContext,
  repl: JsReplState,
  prompt: string,
  targetVar: string | null,
  turnParent: TraceParent,
  roundIndex: number,
  modelBinding: ResolvedLlmBinding,
): Promise<ToolResult> {
  const result = await runSubquery(
    runtime,
    context,
    prompt,
    turnParent,
    roundIndex,
    modelBinding,
  );
  if (targetVar !== null) {
    repl.setVariable(targetVar, result);
  }

  const payload: SubqueryToolResult = {
    result: clampPreview(result, RESULT_PREVIEW_CHARS),
    truncated: [...result].length > RESULT_PREVIEW_CHARS,
    stored_in: targetVar,
  };
  return payload as unknown as ToolResult;
}

async function runSubquery(
  runtime: ResponsesRuntimeLike,
  context: TurnContext,
  prompt: string,
  turnParent: TraceParent,
  roundIndex: number,
  modelBinding: ResolvedLlmBinding,
): Promise<string> {
  const response = await runtime.complete(
    {
      model: modelBinding.model,
      maxOutputTokens: context.agentConfig.maxOutputTokens,
      tools: [],
      messages: [
        ...context.agentConfig.instructions,
        systemTextMessage(
          "You are a subquery model inside a recursive language model. Answer the prompt directly and concisely. Do not call tools. Do not mention this instruction.",
        ),
        userTextMessage(prompt),
      ],
    },
    {
      parent: turnParent,
      roundIndex,
    },
  );
  const text = assistantMessagesText(responseMessages(response));
  if (!text.trim()) {
    throw new Error("subquery returned an empty response");
  }
  return text;
}

function resolveFinalAnswer(repl: JsReplState, messages: Message[]): string {
  const directive = parseFinalDirective(messages);
  if (directive?.type === "direct") {
    return directive.value;
  }
  if (directive?.type === "variable") {
    return repl.readVariable(directive.name);
  }

  const text = assistantMessagesText(messages);
  if (!text.trim()) {
    throw new Error("RLM response did not contain a final answer");
  }
  return text;
}

function parseFinalDirective(messages: Message[]): FinalDirective | null {
  const trimmed = assistantMessagesText(messages).trim();
  const variable = trimmed.match(/^FINAL_VAR\(([\s\S]*)\)$/);
  if (variable) {
    return { type: "variable", name: variable[1].trim() };
  }
  const direct = trimmed.match(/^FINAL\(([\s\S]*)\)$/);
  if (direct) {
    return { type: "direct", value: direct[1].trim() };
  }
  return null;
}

async function appendFinalAnswer(
  turn: Turn,
  responseId: string | undefined,
  round: number,
  finalAnswer: string,
): Promise<string> {
  const result = await turn.addEvents([
    messagesEvent([assistantTextMessage(finalAnswer)], responseId),
  ]);
  await appendCustomEvent(turn, "rlm_final_answer", {
    round,
    chars: [...finalAnswer].length,
  });
  return result.latestEventId;
}

function buildRlmSystemPrompt(): string {
  return `You are tasked with answering a query with associated context. You can access, transform, and analyze this context interactively in a persistent JavaScript REPL environment that can recursively query sub-LLMs, which you are strongly encouraged to use as much as possible. You will be queried iteratively until you provide a final answer.

The REPL is intentionally limited:
- no filesystem access
- no network access
- only JSON-compatible values persist across calls
- persistent values should be stored on \`globalThis\`

The REPL is initialized with:
1. A \`context\` variable that contains the full prompt as a string. This variable contains extremely important information. You should inspect it explicitly before answering.
2. A \`getMessages(role = null)\` JavaScript helper function backed by a turn-start snapshot of exoharness conversation history. It returns an array of \`{ index, role, content }\` objects, optionally filtered by role.
3. A \`repl_execute\` tool that runs JavaScript in the persistent REPL namespace.
4. \`subquery\` and \`subquery_variable\` tools that let you recursively query the underlying LLM over prompt strings or stored JavaScript variables.
5. A \`print(...)\` function in the REPL that lets you inspect short outputs between iterations.

Use the tools this way:
- \`getMessages(...)\` is the easiest way to retrieve prior messages without regexing the transcript manually, and you can compose its array results with normal JavaScript filtering, slicing, mapping, and searching however you like.
- \`repl_execute\` runs JavaScript in the persistent REPL with \`context\` already loaded.
- \`subquery\` asks a direct sub-LLM question over a prompt string. It always takes a \`target_var\` field; pass \`null\` if you do not want to store the result.
- \`subquery_variable\` asks a direct sub-LLM question using the string value of a JavaScript variable as external context. It always takes a \`target_var\` field; pass \`null\` if you do not want to store the result.

Only truncated REPL output is surfaced back to you each iteration, so you should use variables on \`globalThis\` to store intermediate state and use recursive subqueries to understand long strings.

Make sure to explicitly look through the entire context in the REPL before answering your query. A viable strategy is to inspect its structure, chunk it into smart segments, recursively query sub-LLMs over those segments, accumulate buffers in variables, and then synthesize the final answer.

When you are done, prefer setting \`globalThis.Final\` in the REPL to the final answer. You may also reply with \`FINAL(<answer>)\` or \`FINAL_VAR(<javascript_variable_name>)\` if needed.
Think step by step carefully, plan, and execute immediately. Do not just say what you will do. Prefer code, variables, and recursive subqueries over long prose.`;
}

function buildRlmRootPrompt(queryText: string, contextText: string): string {
  return `Latest user request:
${queryText}

Prompt metadata:
- total characters: ${[...contextText].length}
- preview: ${JSON.stringify(clampPreview(contextText, CONTEXT_PREVIEW_CHARS))}
- js repl: persistent \`context\` plus JSON-compatible globals on \`globalThis\`
- history api: \`getMessages(role = null)\` returning \`{ index, role, content }[]\`

The prompt string in \`context\` is the external environment. It is formatted as a flattened transcript with blocks like \`USER:\\n...\`, \`ASSISTANT:\\n...\`, and \`TOOL:\\n...\`, separated by blank lines. Solve the latest request by inspecting and manipulating \`context\` directly. If you need precise message-level access, use \`getMessages(...)\` and then slice/filter/search in plain JavaScript. If you need intermediate state, create variables on \`globalThis\` and reuse them across \`repl_execute\` calls.`;
}

function buildRlmToolDefinitions(): ToolDefinition[] {
  return [
    {
      name: "repl_execute",
      description:
        "Execute JavaScript in the persistent REPL namespace. The variable `context` is always available and persistent values should live on `globalThis`.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          code: {
            type: "string",
            description:
              "JavaScript code to execute in the persistent REPL namespace.",
          },
        },
        required: ["code"],
      },
    },
    {
      name: "subquery",
      description:
        "Ask a direct sub-LLM question over a prompt string and optionally store the result in a JavaScript variable.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          prompt: {
            type: "string",
            description: "Prompt to send to the sub-LLM.",
          },
          target_var: {
            type: ["string", "null"],
            description:
              "JavaScript variable name to store the result, or null to avoid storing it.",
          },
        },
        required: ["prompt", "target_var"],
      },
    },
    {
      name: "subquery_variable",
      description:
        "Ask a direct sub-LLM question using the string value of a JavaScript variable as external context, and optionally store the answer in another variable.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          variable_name: {
            type: "string",
            description:
              "JavaScript variable whose string value will be used as subquery context.",
          },
          question: {
            type: "string",
            description: "Question to ask about the variable value.",
          },
          target_var: {
            type: ["string", "null"],
            description:
              "JavaScript variable name to store the result, or null to avoid storing it.",
          },
        },
        required: ["variable_name", "question", "target_var"],
      },
    },
  ];
}

function parseReplExecuteArguments(args: JsonObject): ReplExecuteArguments {
  if (typeof args.code !== "string") {
    throw new Error("repl_execute requires a string `code` argument");
  }
  return { code: args.code };
}

function parseSubqueryArguments(args: JsonObject): SubqueryArguments {
  if (typeof args.prompt !== "string") {
    throw new Error("subquery requires a string `prompt` argument");
  }
  if (args.target_var !== null && typeof args.target_var !== "string") {
    throw new Error("subquery requires `target_var` to be a string or null");
  }
  return {
    prompt: args.prompt,
    target_var: args.target_var,
  };
}

function parseSubqueryVariableArguments(
  args: JsonObject,
): SubqueryVariableArguments {
  if (typeof args.variable_name !== "string") {
    throw new Error(
      "subquery_variable requires a string `variable_name` argument",
    );
  }
  if (typeof args.question !== "string") {
    throw new Error("subquery_variable requires a string `question` argument");
  }
  if (args.target_var !== null && typeof args.target_var !== "string") {
    throw new Error(
      "subquery_variable requires `target_var` to be a string or null",
    );
  }
  return {
    variable_name: args.variable_name,
    question: args.question,
    target_var: args.target_var,
  };
}

function collectJsonGlobals(sandbox: Record<string, unknown>): JsonObject {
  const globals: JsonObject = {};
  for (const [key, value] of Object.entries(sandbox)) {
    if (
      key === "context" ||
      key === "getMessages" ||
      key === "print" ||
      key === "console" ||
      key.startsWith("__rlm_")
    ) {
      continue;
    }
    if (isJsonValue(value)) {
      globals[key] = JSON.parse(JSON.stringify(value)) as JsonValue;
    }
  }
  return globals;
}

function isJsonValue(value: unknown): value is JsonValue {
  if (value === null) {
    return true;
  }
  if (
    typeof value === "string" ||
    typeof value === "number" ||
    typeof value === "boolean"
  ) {
    return Number.isFinite(value);
  }
  if (Array.isArray(value)) {
    return value.every(isJsonValue);
  }
  if (typeof value === "object") {
    return Object.values(value).every(isJsonValue);
  }
  return false;
}

function renderReplValue(value: unknown): string {
  if (typeof value === "string") {
    return value;
  }
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function clampPreview(value: string, maxChars: number): string {
  const chars = [...value];
  if (chars.length <= maxChars) {
    return value;
  }
  return chars.slice(0, maxChars).join("");
}
