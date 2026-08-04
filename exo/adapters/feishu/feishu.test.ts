import { describe, expect, it } from "vitest";

import {
  feishuSendUuid,
  parseInboundEvent,
  receiveIdTypeForTarget,
} from "./feishu";

function textMessage(
  overrides: Record<string, unknown> = {},
): Record<string, unknown> {
  return {
    message_id: "om_1",
    chat_id: "oc_group",
    chat_type: "group",
    message_type: "text",
    content: JSON.stringify({ text: "hello" }),
    mentions: [{ key: "@_user_1" }],
    ...overrides,
  };
}

const sender = { sender_id: { open_id: "ou_alice", user_id: "u_alice" } };

describe("Feishu inbound parsing", () => {
  it("reads the flattened event body the SDK actually delivers", () => {
    expect(
      parseInboundEvent({ message: textMessage(), sender }, "mentions_only"),
    ).toEqual({
      chatId: "oc_group",
      chatType: "group",
      sender: "ou_alice",
      messageId: "om_1",
      mentionedBot: true,
      text: "hello",
    });
  });

  it("also reads a nested event body", () => {
    const parsed = parseInboundEvent(
      { event: { message: textMessage(), sender } },
      "mentions_only",
    );
    expect(parsed?.text).toBe("hello");
    expect(parsed?.sender).toBe("ou_alice");
  });

  it("applies the mentions_only trigger to group chatter", () => {
    expect(
      parseInboundEvent(
        { message: textMessage({ mentions: [] }), sender },
        "mentions_only",
      ),
    ).toBeNull();
    expect(
      parseInboundEvent(
        { message: textMessage({ mentions: [] }), sender },
        "all_messages",
      ),
    ).not.toBeNull();
  });

  it("always wakes on direct messages", () => {
    expect(
      parseInboundEvent(
        {
          message: textMessage({
            chat_type: "p2p",
            chat_id: "oc_dm",
            mentions: [],
          }),
          sender,
        },
        "mentions_only",
      ),
    ).not.toBeNull();
  });

  it("skips messages it cannot turn into text", () => {
    expect(
      parseInboundEvent(
        { message: textMessage({ message_type: "image" }), sender },
        "all_messages",
      ),
    ).toBeNull();
    expect(
      parseInboundEvent(
        { message: textMessage({ content: "not json" }), sender },
        "all_messages",
      ),
    ).toBeNull();
    expect(
      parseInboundEvent(
        {
          message: textMessage({ content: JSON.stringify({ text: "" }) }),
          sender,
        },
        "all_messages",
      ),
    ).toBeNull();
  });

  it("skips events with no usable message", () => {
    expect(parseInboundEvent(null, "all_messages")).toBeNull();
    expect(parseInboundEvent({ sender }, "all_messages")).toBeNull();
    expect(
      parseInboundEvent(
        { message: textMessage({ chat_id: undefined }), sender },
        "all_messages",
      ),
    ).toBeNull();
  });

  it("falls back to user_id when the sender has no open_id", () => {
    expect(
      parseInboundEvent(
        { message: textMessage(), sender: { sender_id: { user_id: "u_bob" } } },
        "all_messages",
      )?.sender,
    ).toBe("u_bob");
    expect(
      parseInboundEvent({ message: textMessage() }, "all_messages")?.sender,
    ).toBeNull();
  });
});

describe("Feishu outbound targets", () => {
  it("picks the receive id type from the target prefix", () => {
    expect(receiveIdTypeForTarget("ou_alice")).toBe("open_id");
    expect(receiveIdTypeForTarget("oc_group")).toBe("chat_id");
  });
});

describe("Feishu send idempotency key", () => {
  it("passes a delivery id through unchanged", () => {
    const deliveryId = "019f6453-6208-7a41-9c2e-4f1d3b8a5c07";
    expect(feishuSendUuid(deliveryId)).toBe(deliveryId);
  });

  it("is stable for the same delivery id, so a redelivery reuses the key", () => {
    expect(feishuSendUuid("019f6453-6208-7a41-9c2e-4f1d3b8a5c07")).toBe(
      feishuSendUuid("019f6453-6208-7a41-9c2e-4f1d3b8a5c07"),
    );
  });

  it("stays inside Feishu's 50-character limit", () => {
    expect(feishuSendUuid("x".repeat(80))).toHaveLength(50);
  });
});
