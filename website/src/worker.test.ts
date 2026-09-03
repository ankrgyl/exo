import { describe, expect, it } from "vitest";

import type { RelayContext, RelayEnv, RelaySocket } from "./worker.js";
import { RendezvousSession } from "./worker.js";

type FakeSocket = RelaySocket & {
  attachment: { role?: string } | undefined;
  sent: string[];
};

function fakeSocket(role: "agent" | "user"): FakeSocket {
  const socket: FakeSocket = {
    attachment: undefined,
    sent: [],
    readyState: WebSocket.OPEN,
    send(message: string) {
      socket.sent.push(message);
    },
    close() {},
    serializeAttachment(value: unknown) {
      socket.attachment = value as { role?: string };
    },
    deserializeAttachment() {
      return socket.attachment;
    },
  };
  socket.serializeAttachment({ role, connectedAt: 0 });
  return socket;
}

function fakeContext(sockets: FakeSocket[]) {
  const values = new Map<string, unknown>();
  const context: RelayContext = {
    getWebSockets: () => [...sockets],
    acceptWebSocket: () => {},
    storage: {
      get: async (key) => values.get(key),
      put: async (key, value) => {
        values.set(key, value);
      },
      setAlarm: async () => {},
      deleteAll: async () => {
        values.clear();
      },
    },
  };
  return { context, values };
}

function channel(env?: RelayEnv) {
  const agent = fakeSocket("agent");
  const user = fakeSocket("user");
  const { context, values } = fakeContext([agent, user]);
  const session = new RendezvousSession(context, env);
  return { agent, user, context, values, session };
}

function envelope(id?: string): string {
  const frame: Record<string, unknown> = {
    channel: "exo.chat",
    channelId: "chan-0123456789",
    ciphertext: "b3BhcXVl",
    from: "agent",
    nonce: "bm9uY2U0NTY3ODkwMTI",
    seq: 1,
    version: 1,
  };
  if (id !== undefined) {
    frame.id = id;
  }
  return JSON.stringify(frame);
}

function acks(socket: FakeSocket): { id: string; duplicate: boolean }[] {
  return socket.sent
    .map((message) => JSON.parse(message) as Record<string, unknown>)
    .filter((message) => message.channel === "exo.chat.ack")
    .map((message) => ({
      id: message.id as string,
      duplicate: message.duplicate as boolean,
    }));
}

describe("RendezvousSession replay cache", () => {
  it("forwards id-less messages untouched and never acks them", async () => {
    const { agent, user, session } = channel();
    await session.webSocketMessage(agent, "not even json");
    await session.webSocketMessage(agent, envelope());
    expect(user.sent).toEqual(["not even json", envelope()]);
    expect(agent.sent).toEqual([]);
  });

  it("broadcasts a fresh id once and answers the repeat with a duplicate ack", async () => {
    const { agent, user, session } = channel();
    await session.webSocketMessage(agent, envelope("dedupe-id-1"));
    await session.webSocketMessage(agent, envelope("dedupe-id-1"));
    expect(user.sent).toEqual([envelope("dedupe-id-1")]);
    expect(acks(agent)).toEqual([
      { id: "dedupe-id-1", duplicate: false },
      { id: "dedupe-id-1", duplicate: true },
    ]);
  });

  it("treats ids outside the length bounds as legacy traffic", async () => {
    const { agent, user, session } = channel();
    await session.webSocketMessage(agent, envelope("short"));
    await session.webSocketMessage(agent, envelope("short"));
    expect(user.sent.length).toBe(2);
    expect(acks(agent)).toEqual([]);
  });

  it("evicts the oldest id once the ring is full", async () => {
    const { agent, user, session } = channel({ EXOCHAT_REPLAY_LIMIT: "2" });
    await session.webSocketMessage(agent, envelope("dedupe-id-1"));
    await session.webSocketMessage(agent, envelope("dedupe-id-2"));
    await session.webSocketMessage(agent, envelope("dedupe-id-3"));
    await session.webSocketMessage(agent, envelope("dedupe-id-1"));
    expect(user.sent.length).toBe(4);
    expect(acks(agent).map((ack) => ack.duplicate)).toEqual([
      false,
      false,
      false,
      false,
    ]);
  });

  it("forgets an id after the ttl and honestly re-broadcasts", async () => {
    const { agent, user, session } = channel({ EXOCHAT_REPLAY_TTL_MS: "40" });
    await session.webSocketMessage(agent, envelope("dedupe-id-1"));
    await new Promise((resolve) => setTimeout(resolve, 80));
    await session.webSocketMessage(agent, envelope("dedupe-id-1"));
    expect(user.sent.length).toBe(2);
    expect(acks(agent).map((ack) => ack.duplicate)).toEqual([false, false]);
  });

  it("still dedupes after an instance restart when storage survives", async () => {
    const { agent, user, context, session } = channel();
    await session.webSocketMessage(agent, envelope("dedupe-id-1"));
    const woken = new RendezvousSession(context);
    await woken.webSocketMessage(agent, envelope("dedupe-id-1"));
    expect(user.sent).toEqual([envelope("dedupe-id-1")]);
    expect(acks(agent).map((ack) => ack.duplicate)).toEqual([false, true]);
  });

  it("degrades to at-least-once when storage is lost", async () => {
    const { agent, user, context, values, session } = channel();
    await session.webSocketMessage(agent, envelope("dedupe-id-1"));
    values.clear();
    const woken = new RendezvousSession(context);
    await woken.webSocketMessage(agent, envelope("dedupe-id-1"));
    expect(user.sent.length).toBe(2);
    expect(acks(agent).map((ack) => ack.duplicate)).toEqual([false, false]);
  });
});
