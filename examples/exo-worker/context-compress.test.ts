import { describe, expect, it } from "vitest";

import type { Message } from "@exo/harness";

import {
  COMPRESSED_MARKER,
  COMPRESSION_META_PREFIX,
  IMAGE_TOKEN_BUDGET,
  applyCompressionMarkerWindow,
  compressMessagesIfNeeded,
  estimateContentTokens,
  estimateTokens,
  extendToolPairIndices,
  hasToolCallPart,
  parseCompressionMarkerMeta,
  rebuildWithCompression,
  selectCompressIndices,
  selectPinIndices,
  stripVisionImageParts,
} from "./context-compress.js";
import {
  clearLearnedContextWindows,
  isContextWindowError,
  learnContextWindowTokens,
  parseContextWindowFromError,
  resolveContextWindowTokens,
} from "./context-window.js";

function bigUser(n: number, text = "work step"): Message {
  return {
    role: "user",
    content: `${text} ${"detail ".repeat(n)}`,
  };
}

function assistantText(text: string): Message {
  return { role: "assistant", content: text };
}

function toolRound(id: string, name: string, result: unknown): Message[] {
  return [
    {
      role: "assistant",
      content: [
        {
          type: "tool_call",
          tool_call_id: id,
          tool_name: name,
          arguments: {},
        },
      ],
    },
    {
      role: "tool",
      content: [
        {
          type: "tool_result",
          tool_call_id: id,
          tool_name: name,
          output: result,
        },
      ],
    },
  ];
}

describe("context-window", () => {
  it("resolves known model windows and learns from errors", () => {
    clearLearnedContextWindows();
    expect(resolveContextWindowTokens("anthropic:claude-sonnet-5")).toBe(
      1_000_000,
    );
    expect(resolveContextWindowTokens("grok-4.6")).toBe(500_000);
    expect(resolveContextWindowTokens("grok-4.5")).toBe(500_000);
    expect(resolveContextWindowTokens("grok-4.3")).toBe(1_000_000);
    expect(resolveContextWindowTokens("claude-opus-5")).toBe(1_000_000);
    expect(resolveContextWindowTokens("gpt-5.4")).toBe(1_050_000);
    expect(resolveContextWindowTokens("kimi-k2.6")).toBe(262_000);

    learnContextWindowTokens("grok-4.5", 500_000);
    expect(resolveContextWindowTokens("xai:grok-4.5")).toBe(500_000);

    const err = new Error(
      '400 "This model\'s maximum prompt length is 500000 but the request contains 6113480 tokens."',
    );
    expect(isContextWindowError(err)).toBe(true);
    expect(parseContextWindowFromError(err)).toBe(500_000);
  });
});

describe("context-compress selection", () => {
  it("pins developer instructions, first user, and tool pairs", () => {
    const messages: Message[] = [
      { role: "developer", content: "You are ExoWorker." },
      { role: "user", content: "Build me a CLI" },
      ...toolRound("c1", "shell", { ok: true, value: { stdout: "a" } }),
      assistantText("continuing"),
      ...toolRound("c2", "shell", { ok: true, value: { stdout: "b" } }),
    ];

    const pins = selectPinIndices(messages);
    expect(pins.has(0)).toBe(true); // developer
    expect(pins.has(1)).toBe(true); // first user

    // Trailing recent messages are pinned; ensure tool pair extension works
    // when we pin only the tool result.
    const set = new Set<number>([3]); // tool result for c1
    extendToolPairIndices(messages, set);
    expect(set.has(2)).toBe(true); // matching assistant tool_call
    expect(hasToolCallPart(messages[2]!)).toBe(true);
  });

  it("does not split tool_call / tool_result when selecting compress set", () => {
    const messages: Message[] = [
      { role: "developer", content: "sys" },
      { role: "user", content: "client request" },
    ];
    for (let i = 0; i < 20; i++) {
      messages.push(...toolRound(`t${i}`, "shell", { pad: "x".repeat(2_000) }));
      messages.push(bigUser(400, `note-${i}`));
    }

    const total = estimateTokens(messages);
    expect(total).toBeGreaterThan(10_000);

    const indices = selectCompressIndices(messages, 1_000, 5_000);
    expect(indices).not.toBeNull();
    const set = new Set(indices!);
    for (const idx of indices!) {
      const m = messages[idx]!;
      if (m.role === "assistant" && hasToolCallPart(m)) {
        // Matching tool result must also be selected (or pinned out of compress).
        const next = messages[idx + 1];
        if (next?.role === "tool") {
          expect(set.has(idx + 1) || !set.has(idx)).toBe(true);
        }
      }
    }
  });

  it("counts images as a fixed budget, not base64 length", () => {
    const hugeB64 = "A".repeat(2_000_000);
    const tokens = estimateContentTokens([
      { type: "text", text: "see this" },
      { type: "image", image: hugeB64, media_type: "image/png" },
    ]);
    expect(tokens).toBeLessThan(IMAGE_TOKEN_BUDGET + 50);
    expect(tokens).toBeGreaterThanOrEqual(IMAGE_TOKEN_BUDGET);
  });

  it("rebuilds with a single compression marker", () => {
    const messages: Message[] = [
      { role: "developer", content: "sys" },
      { role: "user", content: "client" },
      assistantText("old-1"),
      assistantText("old-2"),
      assistantText("old-3"),
      assistantText("recent"),
    ];
    const rebuilt = rebuildWithCompression(messages, [2, 3, 4], "SUMMARY", {
      compressedMessageCount: 3,
      keptMessageCount: 4,
      estimatedBeforeTokens: 9_000,
      estimatedAfterTokens: 2_000,
      force: false,
    });
    expect(rebuilt).toHaveLength(4);
    expect(rebuilt[0]?.role).toBe("developer");
    expect(rebuilt[1]?.role).toBe("user");
    expect(String(rebuilt[2]?.content)).toContain(COMPRESSED_MARKER);
    expect(String(rebuilt[2]?.content)).toContain(COMPRESSION_META_PREFIX);
    expect(String(rebuilt[2]?.content)).toContain("SUMMARY");
    expect(rebuilt[3]?.content).toBe("recent");
    expect(parseCompressionMarkerMeta(String(rebuilt[2]?.content))).toEqual({
      compressedMessageCount: 3,
      keptMessageCount: 4,
      estimatedBeforeTokens: 9_000,
      estimatedAfterTokens: 2_000,
      force: false,
    });
  });

  it("keeps client request when applying marker window", () => {
    const messages: Message[] = [
      { role: "user", content: "Original client request" },
      assistantText("old work"),
      {
        role: "assistant",
        content: `${COMPRESSED_MARKER}\nprior summary`,
      },
      assistantText("new work"),
    ];
    const windowed = applyCompressionMarkerWindow(messages);
    expect(windowed[0]?.content).toBe("Original client request");
    expect(String(windowed[1]?.content)).toContain(COMPRESSED_MARKER);
    expect(windowed[2]?.content).toBe("new work");
  });

  it("strips vision image parts for context-window retry", () => {
    const messages: Message[] = [
      {
        role: "user",
        content: [
          { type: "text", text: "see this" },
          { type: "image", image: "AAAA", media_type: "image/png" },
        ],
      },
      assistantText("ok"),
    ];
    const { messages: out, strippedCount } = stripVisionImageParts(messages);
    expect(strippedCount).toBe(1);
    expect(JSON.stringify(out)).not.toContain('"type":"image"');
    expect(JSON.stringify(out)).toContain("image omitted");
  });
});

describe("compressMessagesIfNeeded", () => {
  it("compresses with an injected summarizer (no live LLM)", async () => {
    const messages: Message[] = [
      { role: "developer", content: "You are ExoWorker." },
      { role: "user", content: "Please build the thing" },
    ];
    for (let i = 0; i < 30; i++) {
      messages.push(bigUser(800, `step-${i}`));
      messages.push(assistantText(`ack-${i} ${"done ".repeat(200)}`));
    }

    let summarized = false;
    let persisted = false;
    const result = await compressMessagesIfNeeded(messages, {
      model: "claude-sonnet-5",
      maxOutputTokens: 16_000,
      // Tiny budgets so the fixture triggers without megabytes of text.
      budgets: {
        contextWindowTokens: 20_000,
        usableInputTokens: 10_000,
        thresholdTokens: 2_000,
        targetTokens: 3_000,
      },
      summarize: async () => {
        summarized = true;
        return "## Current state\nCompressed fixture summary.";
      },
      persistMarker: async () => {
        persisted = true;
      },
    });

    expect(summarized).toBe(true);
    expect(persisted).toBe(true);
    expect(result.compressed).toBe(true);
    expect(result.estimatedAfterTokens).toBeLessThan(
      result.estimatedBeforeTokens,
    );
    expect(result.meta).not.toBeNull();
    expect(result.meta?.compressedMessageCount).toBeGreaterThan(0);
    expect(result.messages.some((m) => isCompressedMarker(m))).toBe(true);
    const markerText = String(
      result.messages.find((m) => isCompressedMarker(m))?.content,
    );
    expect(parseCompressionMarkerMeta(markerText)?.estimatedBeforeTokens).toBe(
      result.estimatedBeforeTokens,
    );
    // Instructions + client request survive.
    expect(result.messages[0]?.role).toBe("developer");
    expect(
      result.messages.some((m) => m.content === "Please build the thing"),
    ).toBe(true);
  });

  it("force-compresses under threshold for reactive retry", async () => {
    const messages: Message[] = [
      { role: "developer", content: "sys" },
      { role: "user", content: "client" },
    ];
    for (let i = 0; i < 20; i++) {
      messages.push(bigUser(300, `f-${i}`));
      messages.push(assistantText(`a-${i} ${"x".repeat(900)}`));
    }

    const result = await compressMessagesIfNeeded(messages, {
      model: "grok-4.5",
      force: true,
      budgets: {
        thresholdTokens: 50_000_000, // would never trip without force
        targetTokens: 1_000,
      },
      summarize: async () => "forced summary",
    });
    expect(result.compressed).toBe(true);
  });
});

function isCompressedMarker(message: Message): boolean {
  const text =
    typeof message.content === "string"
      ? message.content
      : JSON.stringify(message.content);
  return text.includes(COMPRESSED_MARKER);
}
