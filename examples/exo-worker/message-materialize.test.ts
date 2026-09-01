import { describe, expect, it } from "vitest";

import { toolResultMessage, type Event, type Message } from "@exo/harness";
import { linguaMessagesToResponsesInput } from "@exo/model-runtime/responses";

import {
  HISTORY_TOOL_RESULT_CHAR_CAP,
  LATEST_TOOL_RESULT_CHAR_CAP,
  MAX_VISION_IMAGES_PER_ROUND,
  budgetVisionImages,
  capToolOutputForPrompt,
  estimateDecodedImageBytes,
  hydrateToolResultsForVision,
  materializeExoWorkerEventsToMessages,
  repairLinguaToolPairing,
  splitAssistantToolCallsForResponses,
  stripReasoningParts,
} from "./message-materialize.js";

describe("materializeExoWorkerEventsToMessages", () => {
  it("keeps parallel tool results when names only appear in messages events", () => {
    const events: Event[] = [
      {
        id: "1",
        conversationId: "conversation",
        createdAt: "2026-01-01T00:00:00Z",
        data: {
          type: "messages",
          messages: [
            {
              role: "assistant",
              content: [
                {
                  type: "tool_call",
                  tool_call_id: "call_a",
                  tool_name: "task_tree_update_status",
                  arguments: {},
                },
                {
                  type: "tool_call",
                  tool_call_id: "call_b",
                  tool_name: "task_tree_update_status",
                  arguments: {},
                },
              ],
            },
          ],
        },
      },
      {
        id: "2",
        conversationId: "conversation",
        createdAt: "2026-01-01T00:00:01Z",
        data: {
          type: "tool_result",
          tool_call_id: "call_a",
          result: { ok: true, value: { status: "in_progress" } },
        },
      },
      {
        id: "3",
        conversationId: "conversation",
        createdAt: "2026-01-01T00:00:02Z",
        data: {
          type: "tool_result",
          tool_call_id: "call_b",
          result: { ok: true, value: { status: "completed" } },
        },
      },
    ];

    expect(materializeExoWorkerEventsToMessages(events)).toEqual([
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_a",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
          {
            type: "tool_call",
            tool_call_id: "call_b",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
        ],
      },
      toolResultMessage("call_a", "task_tree_update_status", {
        ok: true,
        value: { status: "in_progress" },
      }),
      toolResultMessage("call_b", "task_tree_update_status", {
        ok: true,
        value: { status: "completed" },
      }),
    ]);
  });
});

describe("repairLinguaToolPairing", () => {
  it("coalesces split assistant rows before pairing tool results", () => {
    const messages: Message[] = [
      {
        role: "assistant",
        content: [{ type: "text", text: "Running tools in parallel." }],
      },
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_a",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
        ],
      },
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_b",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
        ],
      },
      toolResultMessage("call_a", "task_tree_update_status", {
        ok: true,
        value: {},
      }),
      toolResultMessage("call_b", "task_tree_update_status", {
        ok: true,
        value: {},
      }),
    ];

    expect(repairLinguaToolPairing(messages)).toEqual([
      {
        role: "assistant",
        content: [
          { type: "text", text: "Running tools in parallel." },
          {
            type: "tool_call",
            tool_call_id: "call_a",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
          {
            type: "tool_call",
            tool_call_id: "call_b",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
        ],
      },
      toolResultMessage("call_a", "task_tree_update_status", {
        ok: true,
        value: {},
      }),
      toolResultMessage("call_b", "task_tree_update_status", {
        ok: true,
        value: {},
      }),
    ]);
  });

  it("synthesizes missing tool results after parallel assistant tool calls", () => {
    const messages: Message[] = [
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_a",
            tool_name: "task_tree_init",
            arguments: {},
          },
          {
            type: "tool_call",
            tool_call_id: "call_b",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
        ],
      },
      toolResultMessage("call_b", "task_tree_update_status", {
        ok: true,
        value: {},
      }),
    ];

    expect(repairLinguaToolPairing(messages)).toEqual([
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_a",
            tool_name: "task_tree_init",
            arguments: {},
          },
          {
            type: "tool_call",
            tool_call_id: "call_b",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
        ],
      },
      toolResultMessage("call_b", "task_tree_update_status", {
        ok: true,
        value: {},
      }),
      toolResultMessage("call_a", "task_tree_init", {
        ok: false,
        error: "tool result missing from event log; synthesized by ExoWorker",
      }),
    ]);
  });
});

describe("budgetVisionImages", () => {
  it("omits images over the decoded byte budget", () => {
    const huge = "A".repeat(2_000_000); // ~1.5MB decoded
    const decisions = budgetVisionImages(
      [
        {
          toolName: "screenshotUrl",
          base64: huge,
          mediaType: "image/png",
        },
      ],
      { maxBytes: 100_000 },
    );
    expect(decisions).toHaveLength(1);
    expect(decisions[0]?.action).toBe("omit");
    expect(estimateDecodedImageBytes(huge)).toBeGreaterThan(100_000);
  });

  it("caps the number of attaches per round", () => {
    const small = Buffer.from("tiny-png").toString("base64");
    const images = Array.from(
      { length: MAX_VISION_IMAGES_PER_ROUND + 2 },
      (_, i) => ({
        toolName: "previewPresentation",
        base64: small,
        mediaType: "image/png",
        label: `slide ${i + 1}`,
      }),
    );
    const decisions = budgetVisionImages(images);
    const attached = decisions.filter((d) => d.action === "attach");
    const omitted = decisions.filter((d) => d.action === "omit");
    expect(attached).toHaveLength(MAX_VISION_IMAGES_PER_ROUND);
    expect(omitted).toHaveLength(2);
  });
});

describe("capToolOutputForPrompt", () => {
  it("returns the original value when under the cap", () => {
    const output = { ok: true, value: { stdout: "hello" } };
    expect(capToolOutputForPrompt(output, 1_000)).toBe(output);
  });

  it("keeps head and tail when over the cap", () => {
    // Spaces/punctuation so this is not mistaken for base64 by vision strip.
    const body = "alpha line with spaces!\n".repeat(3_000);
    const capped = capToolOutputForPrompt(
      { ok: true, value: { stdout: body } },
      32_000,
      { keepHead: 24_000, keepTail: 4_000 },
    ) as {
      truncated?: boolean;
      originalChars?: number;
      preview?: string;
    };

    expect(capped.truncated).toBe(true);
    expect(capped.originalChars).toBeGreaterThan(32_000);
    expect(capped.preview).toContain("alpha line with spaces!");
    expect(capped.preview).toContain("chars truncated");
    expect(capped.preview!.length).toBeLessThanOrEqual(32_000);
    expect(JSON.stringify(capped).length).toBeLessThan(
      LATEST_TOOL_RESULT_CHAR_CAP + 200,
    );
  });

  it("uses a tighter history budget", () => {
    const body = "beta line with spaces!\n".repeat(1_500);
    const capped = capToolOutputForPrompt(body, HISTORY_TOOL_RESULT_CHAR_CAP, {
      keepHead: 3_000,
      keepTail: 500,
    }) as { truncated?: boolean; preview?: string; originalChars?: number };

    expect(capped.truncated).toBe(true);
    expect(capped.originalChars).toBe(body.length);
    expect(capped.preview!.length).toBeLessThanOrEqual(
      HISTORY_TOOL_RESULT_CHAR_CAP,
    );
  });
});

describe("hydrateToolResultsForVision", () => {
  // Distinct JPEG-shaped payloads so we can assert they never reappear as text.
  const fakeJpegA =
    "/9j/4AAQSkZJRgABAQAAAQABAAD/2wCEAAkGBxISEhUQEhIVFhUVFRUVFRUVFRUWFxUXFhUYHSggGBolGxUVITEhJSkrLi4uFx8zODMtNygtLisBCgoKDg0OGxAQGy0lHyUtLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLf/AABEIAAEAAQMBIgACEQEDEQH/xAAbAAACAwEBAQAAAAAAAAAAAAADBAACBQYBB//EADUQAAIBAwIEBAMEAgMAAAAAAAECAwAEERIhBTFBEyJRYXGBkaGxBjLB0fAHFSNCYvEZ/8QAGQEAAwEBAQAAAAAAAAAAAAAAAAECAwQF/8QAIhEAAgICAgMBAQEAAAAAAAAAAAECERIhAzFBBFEiYXEy/9oADAMBAAIRAxEAPwD1SlKBRSv/2Q==";
  const fakeJpegB =
    "/9j/4AAQSkZJRgABAQAAAQABAAD/2wCEAAkGBxISEhUQEhIVFhUVFRUVFRUVFRUWFxUXFhUYHSggGBolGxUVITEhJSkrLi4uFx8zODMtNygtLisBCgoKDg0OGxAQGy0lHyUtLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLS0tLf/AABEIAAEAAQMBIgACEQEDEQH/xAAbAAACAwEBAQAAAAAAAAAAAAADBAACBQYBB//EADUQAAIBAwIEBAMEAgMAAAAAAAECAwAEERIhBTFBEyJRYXGBkaGxBjLB0fAHFSNCYvEZ/8QAGQEAAwEBAQAAAAAAAAAAAAAAAAECAwQF/8QAIhEAAgICAgMBAQEAAAAAAAAAAAECERIhAzFBBFEiYXEy/9oADAMBAAIRAxEAPwD2TmKBRSv/2Q==";

  it("attaches screenshot bytes as a user image message after the tool result", async () => {
    const conversation = {
      async readArtifactText() {
        return null;
      },
    };

    const messages: Message[] = [
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "shot_1",
            tool_name: "screenshotUrl",
            arguments: {},
          },
        ],
      },
      toolResultMessage("shot_1", "screenshotUrl", {
        success: true,
        title: "Example",
        url: "https://example.com",
        screenshotBase64: fakeJpegA,
      }),
    ];

    const hydrated = await hydrateToolResultsForVision(
      conversation as never,
      messages,
    );
    const repaired = repairLinguaToolPairing(hydrated);

    expect(repaired).toHaveLength(3);
    expect(repaired[1]?.role).toBe("tool");
    const toolOut = (
      repaired[1]!.content as Array<{ output?: Record<string, unknown> }>
    )[0]?.output;
    expect(toolOut?.screenshotBase64).toBeUndefined();
    expect(toolOut?.screenshotBase64Omitted).toBe(true);

    expect(repaired[2]?.role).toBe("user");
    const parts = repaired[2]!.content as Array<{
      type?: string;
      image?: string;
    }>;
    expect(parts.some((p) => p.type === "image" && p.image === fakeJpegA)).toBe(
      true,
    );
  });

  it("does not attach vision for path-only preview results", async () => {
    const artifactJson = JSON.stringify({
      success: true,
      slides: [
        { slideNumber: 1, path: "/tmp/1.png" },
        { slideNumber: 2, path: "/tmp/2.png" },
      ],
      message: "Rendered 2 slide preview(s) to disk.",
    });

    const conversation = {
      async readArtifactText() {
        return artifactJson;
      },
    };

    const previewEnvelope = {
      ok: true,
      toolName: "previewPresentation",
      toolCallId: "prev_vision",
      truncated: true,
      preview: '{"success":true,"slides":2}',
      value: {
        success: true,
        slides: [
          { slideNumber: 1, path: "/tmp/1.png" },
          { slideNumber: 2, path: "/tmp/2.png" },
        ],
      },
      resultArtifact: { artifactId: "art-preview-vision", version: 1 },
    };

    const hydrated = await hydrateToolResultsForVision(conversation as never, [
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "prev_vision",
            tool_name: "previewPresentation",
            arguments: {},
          },
        ],
      },
      toolResultMessage("prev_vision", "previewPresentation", previewEnvelope),
    ]);

    const toolOut = (
      hydrated.find((m) => m.role === "tool")!.content as Array<{
        output?: unknown;
      }>
    )[0]?.output;
    expect(JSON.stringify(toolOut)).toContain("/tmp/1.png");
    expect(JSON.stringify(toolOut)).not.toContain('"imageBase64"');
    const visionCount = hydrated.filter(
      (m) =>
        m.role === "user" &&
        Array.isArray(m.content) &&
        m.content.some((p) => (p as { type?: string }).type === "image"),
    ).length;
    expect(visionCount).toBe(0);
  });

  it("shows nested slide previews once, then strips them from later rounds", async () => {
    let artifactReads = 0;
    const artifactJson = JSON.stringify({
      success: true,
      slides: [
        { slideNumber: 1, path: "/tmp/1.png", imageBase64: fakeJpegA },
        { slideNumber: 2, path: "/tmp/2.png", imageBase64: fakeJpegB },
      ],
    });

    const conversation = {
      async readArtifactText() {
        artifactReads += 1;
        return artifactJson;
      },
    };

    const previewEnvelope = {
      ok: true,
      toolName: "previewPresentation",
      toolCallId: "prev_1",
      truncated: true,
      preview: '{"success":true,"slides":2}',
      value: {
        success: true,
        slides: [{ slideNumber: 1, path: "/tmp/1.png" }],
      },
      resultArtifact: { artifactId: "art-preview-1", version: 1 },
    };

    const round1: Message[] = [
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "prev_1",
            tool_name: "previewPresentation",
            arguments: {},
          },
        ],
      },
      toolResultMessage("prev_1", "previewPresentation", previewEnvelope),
    ];

    const first = await hydrateToolResultsForVision(
      conversation as never,
      round1,
    );
    const firstRepaired = repairLinguaToolPairing(first);
    expect(artifactReads).toBe(1);
    // At most one image attaches per round; base64 must not remain in tool JSON.
    const firstToolOut = (
      firstRepaired.find((m) => m.role === "tool")!.content as Array<{
        output?: unknown;
      }>
    )[0]?.output;
    expect(JSON.stringify(firstToolOut)).not.toContain(fakeJpegA);
    expect(JSON.stringify(firstToolOut)).not.toContain(fakeJpegB);
    const visionCount = firstRepaired.filter(
      (m) =>
        m.role === "user" &&
        Array.isArray(m.content) &&
        m.content.some((p) => (p as { type?: string }).type === "image"),
    ).length;
    expect(visionCount).toBe(MAX_VISION_IMAGES_PER_ROUND);
    expect(JSON.stringify(firstRepaired)).toContain(fakeJpegA);

    // After the model replies, the same tool result is historical — no re-read.
    const round2: Message[] = [
      ...round1,
      {
        role: "assistant",
        content: [{ type: "text", text: "Looks good, continuing." }],
      },
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "shell_1",
            tool_name: "shell",
            arguments: { command: "ls" },
          },
        ],
      },
      toolResultMessage("shell_1", "shell", {
        ok: true,
        value: { stdout: "deck.md\n", stderr: "", exitCode: 0 },
      }),
    ];

    const second = await hydrateToolResultsForVision(
      conversation as never,
      round2,
    );
    expect(artifactReads).toBe(1); // must not rehydrate the old preview
    const serialized = JSON.stringify(second);
    expect(serialized).not.toContain(fakeJpegA);
    expect(serialized).not.toContain(fakeJpegB);
    expect(
      second.some(
        (m) =>
          m.role === "user" &&
          Array.isArray(m.content) &&
          m.content.some((p) => (p as { type?: string }).type === "image"),
      ),
    ).toBe(false);
  });

  it("caps oversized latest tool results and tighter-caps historical ones", async () => {
    const conversation = {
      async readArtifactText() {
        return null;
      },
    };

    // Must not look like base64 — stripAllImagePayloads runs before the cap.
    const hugeStdout = "shell dump line with spaces and punctuation!\n".repeat(
      2_500,
    );
    const trailingOnly: Message[] = [
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "shell_huge",
            tool_name: "shell",
            arguments: { command: "cat big.txt" },
          },
        ],
      },
      toolResultMessage("shell_huge", "shell", {
        ok: true,
        value: { stdout: hugeStdout, stderr: "", exitCode: 0 },
      }),
    ];

    const latestHydrated = await hydrateToolResultsForVision(
      conversation as never,
      trailingOnly,
    );
    const latestOut = (
      latestHydrated.find((m) => m.role === "tool")!.content as Array<{
        output?: { truncated?: boolean; preview?: string };
      }>
    )[0]?.output;
    expect(latestOut?.truncated).toBe(true);
    expect(JSON.stringify(latestOut).length).toBeLessThan(
      LATEST_TOOL_RESULT_CHAR_CAP + 400,
    );
    expect(latestOut?.preview).toContain("chars truncated");

    const afterReply: Message[] = [
      ...trailingOnly,
      {
        role: "assistant",
        content: [{ type: "text", text: "Got it." }],
      },
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "shell_next",
            tool_name: "shell",
            arguments: { command: "echo hi" },
          },
        ],
      },
      toolResultMessage("shell_next", "shell", {
        ok: true,
        value: { stdout: "hi\n", stderr: "", exitCode: 0 },
      }),
    ];

    const historyHydrated = await hydrateToolResultsForVision(
      conversation as never,
      afterReply,
    );
    const historyTool = historyHydrated.find((m) => m.role === "tool")!;
    const historyOut = (
      historyTool.content as Array<{
        output?: { truncated?: boolean; preview?: string };
      }>
    )[0]?.output;
    expect(historyOut?.truncated).toBe(true);
    expect(JSON.stringify(historyOut).length).toBeLessThan(
      HISTORY_TOOL_RESULT_CHAR_CAP + 400,
    );
  });
});

describe("stripReasoningParts", () => {
  it("drops reasoning so coalesce does not mix it with tool_calls", () => {
    const messages: Message[] = [
      { role: "user", content: "run it" },
      {
        role: "assistant",
        content: [{ type: "reasoning", text: "", encrypted_content: "x" }],
      },
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_a",
            tool_name: "shell",
            arguments: { type: "valid", value: { command: "ls" } },
          },
        ],
      },
    ];

    const repaired = repairLinguaToolPairing(stripReasoningParts(messages));
    expect(repaired).toEqual([
      { role: "user", content: "run it" },
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_a",
            tool_name: "shell",
            arguments: { type: "valid", value: { command: "ls" } },
          },
        ],
      },
      toolResultMessage("call_a", "shell", {
        ok: false,
        error: "tool result missing from event log; synthesized by ExoWorker",
      }),
    ]);
  });
});

describe("splitAssistantToolCallsForResponses", () => {
  const toolCall = (id: string, name: string) => ({
    type: "tool_call",
    tool_call_id: id,
    tool_name: name,
    arguments: { type: "valid", value: {} },
  });

  it("preserves every parallel call in Responses input", () => {
    const messages: Message[] = [
      {
        role: "assistant",
        content: [
          toolCall("call_a", "use_skill"),
          toolCall("call_b", "executeCommand"),
        ],
      },
    ];

    const input = linguaMessagesToResponsesInput(
      splitAssistantToolCallsForResponses(messages),
    );
    const callIds = input
      .filter((item) => (item as { type?: string }).type === "function_call")
      .map((item) => (item as { call_id?: string }).call_id);

    expect(callIds).toEqual(["call_a", "call_b"]);
  });

  it("separates assistant text from a tool call", () => {
    const messages: Message[] = [
      {
        role: "assistant",
        content: [
          { type: "text", text: "Checking the workspace." },
          toolCall("call_a", "executeCommand"),
        ],
      },
    ];

    expect(splitAssistantToolCallsForResponses(messages)).toEqual([
      {
        role: "assistant",
        content: [{ type: "text", text: "Checking the workspace." }],
      },
      {
        role: "assistant",
        content: [toolCall("call_a", "executeCommand")],
      },
    ]);
  });
});
