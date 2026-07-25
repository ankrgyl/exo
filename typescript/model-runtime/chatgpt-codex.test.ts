import { describe, expect, it } from "vitest";
import { buildChatGptCodexBody } from "./chatgpt-codex";

describe("OpenAI ChatGPT Codex transport", () => {
  it("uses the current Responses Lite contract for GPT-5.6", () => {
    const body = buildChatGptCodexBody({
      model: "gpt-5.6-terra",
      sessionId: "session-1",
      input: [{ role: "user", content: "hello" }],
      tools: [
        {
          type: "function",
          name: "weather",
          description: "Get the weather",
          parameters: {
            type: "object",
            properties: {},
            additionalProperties: false,
          },
          strict: true,
        },
      ],
    }) as unknown as Record<string, unknown>;

    expect(body).toMatchObject({
      model: "gpt-5.6-terra",
      tool_choice: "auto",
      parallel_tool_calls: false,
      reasoning: { context: "all_turns" },
      store: false,
      stream: true,
      include: ["reasoning.encrypted_content"],
      prompt_cache_key: "session-1",
    });
    expect(body).not.toHaveProperty("metadata");
    expect(body).not.toHaveProperty("tools");
    expect(body.input).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          type: "additional_tools",
          role: "developer",
        }),
      ]),
    );
  });

  it("keeps the standard Codex Responses shape for pre-5.6 models", () => {
    const body = buildChatGptCodexBody({
      model: "gpt-5.4",
      sessionId: "session-1",
      input: [{ role: "user", content: "hello" }],
      instructions: "Be concise.",
    }) as unknown as Record<string, unknown>;

    expect(body).toMatchObject({
      instructions: "Be concise.",
      parallel_tool_calls: false,
      reasoning: {},
      stream: true,
      store: false,
    });
    expect(body).toHaveProperty("tools");
    expect(JSON.stringify(body.input)).not.toContain("Be concise.");
  });
});
