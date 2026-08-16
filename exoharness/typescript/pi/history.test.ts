import { describe, expect, it } from "vitest";

import { messageText, type Message } from "@exo/harness";

import { capPiHistory } from "./history";

describe("capPiHistory", () => {
  it("keeps recent messages within the total history budget", () => {
    const messages = ["oldest", "second", "third", "newest"].map(
      (label): Message => ({
        role: "user",
        content: `${label}:${"x".repeat(7_990 - label.length)}`,
      }),
    );

    const capped = capPiHistory(messages);

    expect(capped.sourceMessageCount).toBe(4);
    expect(capped.messages).toHaveLength(3);
    expect(capped.droppedMessageCount).toBe(1);
    expect(messageText(capped.messages[0])).toContain("second:");
    expect(messageText(capped.messages[2])).toContain("newest:");
    expect(capped.textChars).toBeLessThanOrEqual(24_000);
  });

  it("keeps a contiguous suffix instead of backfilling older short messages", () => {
    const capped = capPiHistory([
      { role: "user", content: "old but short" },
      { role: "assistant", content: "blocking".repeat(1_000) },
      { role: "user", content: "a".repeat(7_990) },
      { role: "assistant", content: "b".repeat(7_990) },
      { role: "user", content: "c".repeat(7_990) },
      { role: "assistant", content: "newest" },
    ]);

    expect(capped.messages).toHaveLength(4);
    expect(capped.droppedMessageCount).toBe(2);
    expect(messageText(capped.messages[0])).toBe("a".repeat(7_990));
    expect(messageText(capped.messages[3])).toBe("newest");
  });

  it("truncates normal messages and tool results at separate limits", () => {
    const capped = capPiHistory([
      { role: "assistant", content: "a".repeat(10_000) },
      { role: "tool", content: "b".repeat(10_000) },
    ]);

    expect(capped.truncatedMessageCount).toBe(2);
    expect(messageText(capped.messages[0])).toHaveLength(8_000);
    expect(messageText(capped.messages[1])).toHaveLength(4_000);
    expect(messageText(capped.messages[0])).toContain("[truncated 2000");
    expect(messageText(capped.messages[1])).toContain("[truncated 6000");
  });

  it("counts truncation only for retained messages", () => {
    const capped = capPiHistory([
      { role: "assistant", content: "old".repeat(10_000) },
      { role: "user", content: "a".repeat(8_000) },
      { role: "assistant", content: "b".repeat(8_000) },
      { role: "user", content: "c".repeat(8_000) },
    ]);

    expect(capped.droppedMessageCount).toBe(1);
    expect(capped.truncatedMessageCount).toBe(0);
  });

  it("always retains at least the newest message", () => {
    const capped = capPiHistory([
      { role: "user", content: "x".repeat(50_000) },
    ]);

    expect(capped.messages).toHaveLength(1);
    expect(capped.droppedMessageCount).toBe(0);
    expect(messageText(capped.messages[0])).toHaveLength(8_000);
  });
});
