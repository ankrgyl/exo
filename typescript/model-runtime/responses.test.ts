import { describe, expect, it } from "vitest";
import type { Response } from "openai/resources/responses/responses";

import {
  AnthropicRuntime,
  ChatCompletionsRuntime,
  isAnthropicModel,
  isOpenRouterBinding,
  modelRequiresResponsesApi,
  costFromAnthropicStreamEvent,
  openAiCredentials,
  responseToLinguaEvents,
  responseToolCalls,
  walkUsagePath,
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

  it("sends exactly the declared credential, never both schemes", () => {
    // x-api-key: real key rides the header, Authorization is suppressed.
    expect(
      openAiCredentials({ apiKey: "provider-key", auth: "x-api-key" }),
    ).toEqual({
      apiKey: "unauthenticated",
      defaultHeaders: { authorization: null, "x-api-key": "provider-key" },
    });
    // none: no credential headers at all (even when a key leaked through).
    expect(openAiCredentials({ apiKey: "provider-key", auth: "none" })).toEqual(
      {
        apiKey: "unauthenticated",
        defaultHeaders: { authorization: null },
      },
    );
    // provider-declared without a key: hard error, mirroring the Rust
    // runtime — never silently unauthenticated.
    expect(() => openAiCredentials({ format: "chat-completions" })).toThrow(
      /missing an API key/,
    );
    // bearer stays a plain SDK key; legacy bindings keep env fallback.
    expect(openAiCredentials({ apiKey: "k", auth: "bearer" })).toEqual({
      apiKey: "k",
    });
    expect(openAiCredentials({ model: "gpt-4o" })).toEqual({
      apiKey: undefined,
    });
  });

  it("reads provider-reported cost via the declared usage path", () => {
    const usage = {
      prompt_tokens: 14,
      opper: { cost: { total: 0.0000035 } },
    };
    expect(walkUsagePath(usage, ["opper", "cost", "total"])).toBe(0.0000035);
    expect(walkUsagePath(usage, ["cost"])).toBeNull();
    expect(walkUsagePath(usage, ["opper", "cost", "missing"])).toBeNull();
    expect(walkUsagePath(null, ["cost"])).toBeNull();
  });

  it("captures provider cost from raw anthropic stream events", () => {
    // The SDK's accumulated finalMessage().usage strips unknown fields, so
    // the cost extension must be read off the raw SSE events.
    const path = ["opper", "cost", "total"];
    expect(
      costFromAnthropicStreamEvent(
        {
          type: "message_start",
          message: {
            usage: { input_tokens: 14, opper: { cost: { total: 0.0000042 } } },
          },
        },
        path,
      ),
    ).toBe(0.0000042);
    expect(
      costFromAnthropicStreamEvent(
        {
          type: "message_delta",
          usage: { output_tokens: 9, opper: { cost: { total: 0.0000051 } } },
        },
        path,
      ),
    ).toBe(0.0000051);
    expect(
      costFromAnthropicStreamEvent(
        {
          type: "content_block_delta",
          delta: { type: "text_delta", text: "x" },
        },
        path,
      ),
    ).toBeNull();
    expect(costFromAnthropicStreamEvent(null, path)).toBeNull();
  });

  it("prefers provider-reported cost over the price-table estimate", () => {
    const response = {
      id: "resp_1",
      model: "some/model",
      output: [
        {
          type: "message",
          role: "assistant",
          content: [{ type: "output_text", text: "hi", annotations: [] }],
        },
      ],
      usage: {
        input_tokens: 10,
        output_tokens: 2,
        total_tokens: 12,
        provider_cost_usd: 0.0000042,
      },
    } as unknown as Response;

    expect(responseToLinguaEvents(response)).toContainEqual({
      type: "messages",
      messages: expect.any(Array),
      response_id: undefined,
      usage: expect.objectContaining({ cost_usd: 0.0000042 }),
    });
  });

  it("honors a declared provider wire format over the heuristics", () => {
    // chat-completions wins over a claude model name (native path otherwise).
    expect(
      runtimeFromModelBinding(undefined, {
        model: "anthropic/claude-sonnet-4-6",
        apiKey: "key",
        baseUrl: "https://api.opper.ai/v3/compat",
        format: "chat-completions",
      }),
    ).toBeInstanceOf(ChatCompletionsRuntime);
    // responses wins over a chat-only model name.
    expect(
      runtimeFromModelBinding(undefined, {
        model: "deepseek-chat",
        apiKey: "key",
        format: "responses",
      }),
    ).toBeInstanceOf(ResponsesRuntime);
    // anthropic format routes to the native Messages runtime.
    expect(
      runtimeFromModelBinding(undefined, {
        model: "some-proxy-model",
        apiKey: "key",
        format: "anthropic",
      }),
    ).toBeInstanceOf(AnthropicRuntime);
    // auth none constructs without a credential (headers explicitly omitted).
    expect(
      runtimeFromModelBinding(undefined, {
        model: "local-model",
        format: "anthropic",
        auth: "none",
      }),
    ).toBeInstanceOf(AnthropicRuntime);
    // Absent format falls back to the existing heuristics.
    expect(
      runtimeFromModelBinding(undefined, {
        model: "gpt-5.4",
        apiKey: "key",
      }),
    ).toBeInstanceOf(ResponsesRuntime);
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
});
