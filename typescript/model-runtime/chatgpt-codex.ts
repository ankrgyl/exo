import type {
  ResponseCreateParamsStreaming,
  ResponseInput,
  Tool,
} from "openai/resources/responses/responses";

import type { Message } from "../harness";

export const CHATGPT_CODEX_BASE_URL = "https://chatgpt.com/backend-api/codex";

export interface ChatGptCodexRequest {
  model: string;
  sessionId?: string;
  input: ResponseInput;
  instructions?: string;
  tools?: Tool[];
}

interface ChatGptCodexBody {
  model: string;
  input: ResponseInput;
  instructions?: string;
  tools?: Tool[];
  tool_choice: "auto";
  parallel_tool_calls: boolean;
  reasoning: {
    context?: "all_turns";
  };
  include: ["reasoning.encrypted_content"];
  prompt_cache_key: string;
  store: false;
  stream: true;
}

export function buildChatGptCodexBody(
  request: ChatGptCodexRequest,
): ResponseCreateParamsStreaming {
  const responsesLite = chatGptCodexUsesResponsesLite(request.model);
  const tools = request.tools ?? [];
  const bodyInput = responsesLite
    ? prependCodexAdditionalTools(request.input, tools)
    : request.input;

  const body: ChatGptCodexBody = {
    model: request.model,
    input: bodyInput,
    ...(request.instructions ? { instructions: request.instructions } : {}),
    ...(responsesLite ? {} : { tools }),
    tool_choice: "auto",
    parallel_tool_calls: responsesLite ? false : tools.length > 0,
    reasoning: responsesLite ? { context: "all_turns" } : {},
    store: false,
    stream: true,
    include: ["reasoning.encrypted_content"],
    prompt_cache_key: request.sessionId ?? "exo",
  };
  return body as unknown as ResponseCreateParamsStreaming;
}

export function buildChatGptCodexHeaders(
  request: Pick<ChatGptCodexRequest, "model" | "sessionId">,
): Record<string, string> {
  const sessionId = request.sessionId ?? "exo";
  return {
    "session-id": sessionId,
    "x-client-request-id": sessionId,
    accept: "text/event-stream",
    "content-type": "application/json",
    ...(chatGptCodexUsesResponsesLite(request.model)
      ? { "x-openai-internal-codex-responses-lite": "true" }
      : {}),
  };
}

export function chatGptCodexUsesResponsesLite(model: string): boolean {
  return /^gpt-5\.6(?:-|$)/i.test(model);
}

export function partitionChatGptCodexMessages(
  model: string,
  messages: Message[] | undefined,
): {
  inputMessages: Message[] | undefined;
  instructionMessages: Message[];
} {
  if (chatGptCodexUsesResponsesLite(model)) {
    return { inputMessages: messages, instructionMessages: [] };
  }
  const firstInput = (messages ?? []).findIndex(
    (message) => message.role !== "system" && message.role !== "developer",
  );
  if (firstInput < 0) {
    return { inputMessages: [], instructionMessages: messages ?? [] };
  }
  return {
    inputMessages: messages?.slice(firstInput),
    instructionMessages: messages?.slice(0, firstInput) ?? [],
  };
}

function prependCodexAdditionalTools(
  input: ResponseInput,
  tools: Tool[],
): ResponseInput {
  if (tools.length === 0) {
    return input;
  }
  return [
    {
      type: "additional_tools",
      role: "developer",
      tools,
    },
    ...(Array.isArray(input) ? input : [input]),
  ] as ResponseInput;
}
