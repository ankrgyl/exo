import { describe, expect, it } from "vitest";
import type { Response } from "openai/resources/responses/responses";

import {
  AnthropicRuntime,
  ChatCompletionsRuntime,
  isAnthropicModel,
  isOpenRouterBinding,
  messagesToChatMessages,
  modelRequiresResponsesApi,
  responseToLinguaEvents,
  responseToolCalls,
  runtimeFromModelBinding,
  ResponsesRuntime,
} from "./responses";

describe("model runtime dispatch", () => {
  it("matches the Responses-required model families", () => {
    for (const model of [
      "o1-pro",
      "o3-pro",
      "gpt-5-pro",
      "gpt-5.3",
      "gpt-5.4",
      "gpt-5-codex",
      "gpt-5.1-codex-mini",
    ]) {
      expect(modelRequiresResponsesApi(model)).toBe(true);
    }

    for (const model of [
      "deepseek-chat",
      "gpt-4o",
      "gpt-5",
      "gpt-5.1",
      "gpt-5.2-chat-latest",
    ]) {
      expect(modelRequiresResponsesApi(model)).toBe(false);
    }
  });

  it("dispatches chat-only models away from Responses", () => {
    expect(
      runtimeFromModelBinding(undefined, {
        model: "deepseek-chat",
        apiKey: "key",
      }),
    ).toBeInstanceOf(ChatCompletionsRuntime);
    expect(
      runtimeFromModelBinding(undefined, {
        model: "gpt-5.4",
        apiKey: "key",
      }),
    ).toBeInstanceOf(ResponsesRuntime);
  });

  it("dispatches claude models to the native Anthropic runtime", () => {
    expect(isAnthropicModel("claude-sonnet-4-6")).toBe(true);
    expect(isAnthropicModel("gpt-5.4")).toBe(false);
    expect(isAnthropicModel("us.anthropic.claude-sonnet-4-6")).toBe(false);
    expect(
      runtimeFromModelBinding(undefined, {
        model: "claude-sonnet-4-6",
        apiKey: "key",
      }),
    ).toBeInstanceOf(AnthropicRuntime);
  });

  it("routes OpenRouter bindings through chat completions by base URL", () => {
    expect(
      isOpenRouterBinding({ baseUrl: "https://openrouter.ai/api/v1" }),
    ).toBe(true);
    expect(isOpenRouterBinding({ baseUrl: null })).toBe(false);
    // A Responses-looking model name over OpenRouter still uses chat completions.
    expect(
      runtimeFromModelBinding(undefined, {
        model: "openai/gpt-5-pro",
        apiKey: "key",
        baseUrl: "https://openrouter.ai/api/v1",
      }),
    ).toBeInstanceOf(ChatCompletionsRuntime);
  });
});

describe("response tool-call parsing", () => {
  it("attaches response usage to message events", () => {
    const response = {
      id: "resp_1",
      model: "gpt-5.4",
      output: [
        {
          type: "message",
          role: "assistant",
          content: [
            {
              type: "output_text",
              text: "hello",
              annotations: [],
            },
          ],
        },
      ],
      usage: {
        input_tokens: 12,
        output_tokens: 5,
        total_tokens: 17,
        input_tokens_details: {
          cached_tokens: 3,
        },
        output_tokens_details: {
          reasoning_tokens: 2,
        },
      },
    } as unknown as Response;

    expect(responseToLinguaEvents(response)).toContainEqual({
      type: "messages",
      messages: expect.any(Array),
      response_id: undefined,
      usage: expect.objectContaining({
        model: "gpt-5.4",
        prompt_tokens: 12,
        completion_tokens: 5,
        prompt_cached_tokens: 3,
        completion_reasoning_tokens: 2,
      }),
    });
  });

  it("turns malformed function arguments into tool result errors", () => {
    const response = {
      id: "resp_1",
      output: [
        {
          type: "function_call",
          call_id: "call_1",
          name: "shell",
          arguments: '{"command":',
        },
      ],
    } as unknown as Response;

    expect(responseToolCalls(response)).toEqual([]);
    expect(responseToLinguaEvents(response)).toContainEqual({
      type: "tool_result",
      tool_call_id: "call_1",
      result: {
        ok: false,
        error: expect.stringContaining("Invalid JSON arguments for shell"),
      },
    });
  });

  it("unwraps the lingua {type:valid,value} envelope on tool arguments", () => {
    const response = {
      id: "resp_1",
      output: [
        {
          type: "function_call",
          call_id: "call_1",
          name: "shell",
          arguments: '{"type":"valid","value":"{\\"command\\":\\"ls -la\\"}"}',
        },
      ],
    } as unknown as Response;

    expect(responseToolCalls(response)).toEqual([
      {
        toolCallId: "call_1",
        request: { functionName: "shell", arguments: { command: "ls -la" } },
      },
    ]);
  });

  it("unwraps serde_json::private::Number leaks inside tool arguments", () => {
    const response = {
      id: "resp_1",
      output: [
        {
          type: "function_call",
          call_id: "call_1",
          name: "web_fetch",
          arguments:
            '{"type":"valid","value":"{\\"url\\":\\"https://x.test/\\",\\"maxChars\\":{\\"$serde_json::private::Number\\":\\"3000\\"}}"}',
        },
      ],
    } as unknown as Response;

    expect(responseToolCalls(response)).toEqual([
      {
        toolCallId: "call_1",
        request: {
          functionName: "web_fetch",
          arguments: { url: "https://x.test/", maxChars: 3000 },
        },
      },
    ]);
  });

  it("leaves well-formed tool arguments untouched", () => {
    const response = {
      id: "resp_1",
      output: [
        {
          type: "function_call",
          call_id: "call_1",
          name: "web_search",
          arguments: '{"query":"rust release","count":5}',
        },
      ],
    } as unknown as Response;

    expect(responseToolCalls(response)).toEqual([
      {
        toolCallId: "call_1",
        request: {
          functionName: "web_search",
          arguments: { query: "rust release", count: 5 },
        },
      },
    ]);
  });
});

describe("chat-completions history replay", () => {
  it("unwraps the lingua ToolCallArguments enum when replaying tool calls", () => {
    const chatMessages = messagesToChatMessages([
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_1",
            tool_name: "shell",
            arguments: { type: "valid", value: { command: "ls -la" } },
          },
        ],
      },
    ]);

    expect(chatMessages).toEqual([
      {
        role: "assistant",
        content: null,
        tool_calls: [
          {
            id: "call_1",
            type: "function",
            function: {
              name: "shell",
              arguments: '{"command":"ls -la"}',
            },
          },
        ],
      },
    ]);
  });

  it("replays invalid arguments as the raw string the model produced", () => {
    const [assistant] = messagesToChatMessages([
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_1",
            tool_name: "shell",
            arguments: { type: "invalid", value: "{not json" },
          },
        ],
      },
    ]);

    expect(
      assistant.role === "assistant" &&
        assistant.tool_calls?.[0]?.type === "function" &&
        assistant.tool_calls[0].function.arguments,
    ).toBe("{not json");
  });

  it("passes through plain-object arguments unchanged", () => {
    const [assistant] = messagesToChatMessages([
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_1",
            tool_name: "shell",
            arguments: { command: "ls" },
          },
        ],
      },
    ]);

    expect(
      assistant.role === "assistant" &&
        assistant.tool_calls?.[0]?.type === "function" &&
        assistant.tool_calls[0].function.arguments,
    ).toBe('{"command":"ls"}');
  });
});
