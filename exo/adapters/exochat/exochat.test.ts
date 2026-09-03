import { describe, expect, it } from "vitest";

import { parseSendAck, sendDedupeId } from "./exochat";

describe("sendDedupeId", () => {
  const commandId = "0198c4f1-7b2a-7c3d-8e4f-5a6b7c8d9e0f";

  it("is deterministic across restarts", () => {
    // A replayed command must present the same id as the attempt that
    // crashed, or the relay sees an unrelated send and broadcasts again.
    expect(sendDedupeId(commandId)).toBe(sendDedupeId(commandId));
  });

  it("differs per command", () => {
    expect(sendDedupeId("command-a")).not.toBe(sendDedupeId("command-b"));
  });

  it("does not leak the command id to the relay", () => {
    expect(sendDedupeId(commandId)).not.toContain(commandId);
  });

  it("fits the relay's id bounds", () => {
    const id = sendDedupeId(commandId);
    expect(id.length).toBeGreaterThanOrEqual(8);
    expect(id.length).toBeLessThanOrEqual(128);
  });
});

describe("parseSendAck", () => {
  it("accepts a relay ack", () => {
    expect(
      parseSendAck({
        channel: "exo.chat.ack",
        id: "dedupe-id-1",
        duplicate: true,
        at: 123,
      }),
    ).toEqual({ id: "dedupe-id-1", duplicate: true });
  });

  it("rejects envelopes, presence, and malformed acks", () => {
    expect(parseSendAck({ channel: "exo.chat", id: "dedupe-id-1" })).toBeNull();
    expect(
      parseSendAck({ channel: "rendezvous", type: "presence" }),
    ).toBeNull();
    expect(parseSendAck({ channel: "exo.chat.ack", id: 7 })).toBeNull();
    expect(parseSendAck(null)).toBeNull();
    expect(parseSendAck("exo.chat.ack")).toBeNull();
  });
});
