// Pi harness: runs the Pi coding agent (https://pi.dev) inside an exo sandbox.
//
// Each turn spawns `pi --mode json`, seeded from exo's history, and every step
// pi reports is appended back.

import {
  assistantTextMessage,
  defineHarness,
  materializeConversationMessages,
  messageText,
  messagesToTranscript,
  toolRequestedEvent,
  toolResultEvent,
  messagesEvent,
  type JsonValue,
  type Message,
  type TurnContext,
} from "@exo/harness";

import {
  appendEvents,
  pickEnv,
  resolveLlmBinding,
  type ResolvedLlmBinding,
} from "@exo/model-runtime/shared";

const PI_BIN = "pi";
const DEFAULT_PROVIDER = "openai";
const PROVIDER_KEY_VARIABLES: Record<string, string> = {
  openai: "OPENAI_API_KEY",
  anthropic: "ANTHROPIC_API_KEY",
  google: "GEMINI_API_KEY",
};

// exo names a model on its own; `provider/model` selects one explicitly.
function splitModelReference(reference: string): [string, string] {
  const slash = reference.indexOf("/");
  return slash === -1
    ? [DEFAULT_PROVIDER, reference]
    : [reference.slice(0, slash), reference.slice(slash + 1)];
}

function systemPrompt(context: TurnContext): string | null {
  const instructions = context.agentConfig.instructions
    .map(messageText)
    .filter(Boolean)
    .join("\n\n");
  return instructions || null;
}

function piCommand(context: TurnContext): string[] {
  const [provider, model] = splitModelReference(context.agentConfig.model);
  const command = [
    PI_BIN,
    "--mode",
    "json",
    "--print",
    "--no-session", // exo owns the convo, so no need to look for a session
    "--model",
    `${provider}/${model}`,
  ];
  const instructions = systemPrompt(context);
  if (instructions) {
    command.push("--system-prompt", instructions);
  }
  return command;
}

// The key comes from exo's binding rather than the env.
function piEnv(
  context: TurnContext,
  binding: ResolvedLlmBinding,
): Record<string, string> {
  const [provider] = splitModelReference(context.agentConfig.model);
  const env = pickEnv((key) => key.startsWith("PI_"));
  const keyVariable = PROVIDER_KEY_VARIABLES[provider];
  if (keyVariable && binding.apiKey) {
    env[keyVariable] = binding.apiKey;
  }
  return env;
}

// pi emits one JSON object per line; a chunk can split a line anywhere.
async function* jsonLines(
  stream: ReadableStream<string>,
): AsyncGenerator<Record<string, unknown>> {
  const reader = stream.getReader();
  let buffer = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    buffer += value;
    let newline = buffer.indexOf("\n");
    while (newline !== -1) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      if (line) {
        try {
          yield JSON.parse(line) as Record<string, unknown>;
        } catch {
          // pi writes only JSON here; anything else is not ours to interpret.
        }
      }
      newline = buffer.indexOf("\n");
    }
  }
}

function assistantText(message: Record<string, unknown>): string {
  const content = message.content;
  if (!Array.isArray(content)) {
    return "";
  }
  return content
    .map((part) => {
      const record = part as Record<string, unknown>;
      return record.type === "text" && typeof record.text === "string"
        ? record.text
        : "";
    })
    .join("");
}

// Translate Pi's cost tracking into the local record.
function usageRecord(
  context: TurnContext,
  message: Record<string, unknown>,
): Record<string, JsonValue> | undefined {
  const usage = message.usage as Record<string, unknown> | undefined;
  if (!usage) {
    return undefined;
  }
  const number = (value: unknown): number | undefined =>
    typeof value === "number" ? value : undefined;
  const cost = usage.cost as Record<string, unknown> | undefined;
  const record: Record<string, JsonValue> = {
    model: String(message.model ?? context.agentConfig.model),
  };
  const fields: Array<[string, number | undefined]> = [
    ["prompt_tokens", number(usage.input)],
    ["completion_tokens", number(usage.output)],
    ["prompt_cached_tokens", number(usage.cacheRead)],
    ["prompt_cache_creation_tokens", number(usage.cacheWrite)],
    ["completion_reasoning_tokens", number(usage.reasoning)],
    ["cost_usd", number(cost?.total)],
  ];
  for (const [key, value] of fields) {
    if (value !== undefined) {
      record[key] = value;
    }
  }
  return record;
}

// map pi's steps to exo events: a tool call, its result, and assistant text.
function eventsForPiEvent(
  context: TurnContext,
  event: Record<string, unknown>,
) {
  const type = event.type;

  if (type === "tool_execution_start") {
    return [
      toolRequestedEvent({
        toolCallId: String(event.toolCallId),
        request: {
          functionName: String(event.toolName),
          arguments: (event.args ?? {}) as Record<string, JsonValue>,
        },
      }),
    ];
  }

  if (type === "tool_execution_end") {
    return [
      toolResultEvent(String(event.toolCallId), {
        result: (event.result ?? null) as JsonValue,
        is_error: event.isError === true,
      }),
    ];
  }

  if (type === "message_end") {
    const message = event.message as Record<string, unknown> | undefined;
    if (message?.role === "assistant") {
      const text = assistantText(message);
      if (text) {
        return [
          messagesEvent(
            [assistantTextMessage(text)],
            undefined,
            usageRecord(context, message),
          ),
        ];
      }
    }
  }

  return [];
}

const harness = defineHarness({
  tools: [],

  async runTurn(context) {
    const history = await materializeConversationMessages(
      context.exoharness.current.conversation,
    );
    const prompt = messagesToTranscript(
      history.filter(
        (message: Message) =>
          message.role !== "system" && message.role !== "developer",
      ),
    );

    const binding = await resolveLlmBinding(context);
    const process = await context.startSandboxProcess({
      command: piCommand(context),
      env: piEnv(context, binding),
    });

    // The prompt goes over stdin (otherwise could be too long for argv).
    await process.writeStdin(prompt);
    await process.closeStdin();

    let lastText = "";
    for await (const event of jsonLines(process.stdout)) {
      const events = eventsForPiEvent(context, event);
      if (events.length > 0) {
        await appendEvents(context, events);
      }
      if (event.type === "message_end") {
        const message = event.message as Record<string, unknown> | undefined;
        if (message?.role === "assistant") {
          lastText = assistantText(message) || lastText;
        }
      }
    }

    if (lastText) {
      await context.stream.text(lastText);
    }

    const exitCode = await process.wait();
    if (exitCode !== 0) {
      throw new Error(`pi exited with status ${exitCode}`);
    }
  },
});

export default harness;
