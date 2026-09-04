import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { createReconnectingWebSocket } from "./reconnecting-websocket";

type SocketListener = (event: never) => void;

class FakeWebSocket {
  readonly listeners = new Map<string, SocketListener[]>();
  readonly sent: string[] = [];
  closeCalls = 0;
  readyState = 0;

  addEventListener(type: string, listener: SocketListener) {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  close() {
    this.closeCalls += 1;
    this.readyState = 3;
    this.emit("close", { code: 1000, reason: "" });
  }

  emit(type: string, event: unknown) {
    for (const listener of this.listeners.get(type) ?? []) {
      listener(event as never);
    }
  }

  error() {
    this.emit("error", {});
  }

  message(data: string) {
    this.emit("message", { data });
  }

  open() {
    this.readyState = 1;
    this.emit("open", {});
  }

  send(data: string) {
    this.sent.push(data);
  }

  serverClose(code: number, reason = "") {
    this.readyState = 3;
    this.emit("close", { code, reason });
  }
}

function setup() {
  const sockets: FakeWebSocket[] = [];
  const callbacks = {
    onConnecting: vi.fn(),
    onError: vi.fn(),
    onMessage: vi.fn(),
    onOpen: vi.fn(),
    onReconnectScheduled: vi.fn(),
    onTerminalClose: vi.fn(),
  };
  const connection = createReconnectingWebSocket({
    createSocket: () => {
      const socket = new FakeWebSocket();
      sockets.push(socket);
      return socket as unknown as WebSocket;
    },
    ...callbacks,
  });
  return { callbacks, connection, sockets };
}

describe("reconnecting WebSocket", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("starts once and forwards events from the active socket", () => {
    const { callbacks, connection, sockets } = setup();

    connection.start();
    connection.start();

    expect(sockets).toHaveLength(1);
    expect(callbacks.onConnecting).toHaveBeenCalledOnce();
    expect(callbacks.onConnecting).toHaveBeenCalledWith({ attempt: 0 });
    expect(connection.send("too early")).toBe(false);

    sockets[0].open();
    sockets[0].message("hello");
    sockets[0].error();

    expect(callbacks.onOpen).toHaveBeenCalledWith({ reconnected: false });
    expect(callbacks.onMessage).toHaveBeenCalledWith({ data: "hello" });
    expect(callbacks.onError).toHaveBeenCalledOnce();
    expect(connection.send("ready")).toBe(true);
    expect(sockets[0].sent).toEqual(["ready"]);
  });

  it("reconnects with exponential backoff and resets after opening", () => {
    const { callbacks, connection, sockets } = setup();
    connection.start();

    sockets[0].serverClose(1006);
    expect(callbacks.onReconnectScheduled).toHaveBeenLastCalledWith({
      attempt: 1,
      delayMs: 1_000,
      event: { code: 1006, reason: "" },
    });

    vi.advanceTimersByTime(999);
    expect(sockets).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(sockets).toHaveLength(2);
    expect(callbacks.onConnecting).toHaveBeenLastCalledWith({ attempt: 1 });

    sockets[1].serverClose(1006);
    vi.advanceTimersByTime(2_000);
    expect(sockets).toHaveLength(3);
    expect(callbacks.onConnecting).toHaveBeenLastCalledWith({ attempt: 2 });

    sockets[2].open();
    expect(callbacks.onOpen).toHaveBeenLastCalledWith({ reconnected: true });
    sockets[2].serverClose(1006);
    expect(callbacks.onReconnectScheduled).toHaveBeenLastCalledWith({
      attempt: 1,
      delayMs: 1_000,
      event: { code: 1006, reason: "" },
    });
  });

  it("caps the reconnect delay at thirty seconds", () => {
    const { callbacks, connection, sockets } = setup();
    connection.start();

    for (let index = 0; index < 8; index += 1) {
      sockets[index].serverClose(1006);
      const { delayMs } = callbacks.onReconnectScheduled.mock.calls[index][0];
      vi.advanceTimersByTime(delayMs);
    }

    expect(
      callbacks.onReconnectScheduled.mock.calls.map(
        ([reconnect]) => reconnect.delayMs,
      ),
    ).toEqual([1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000]);
  });

  it("does not fight a newer connection for the same role", () => {
    const { callbacks, connection, sockets } = setup();
    connection.start();

    sockets[0].serverClose(1000, "Replaced by a newer connection");
    vi.advanceTimersByTime(60_000);

    expect(sockets).toHaveLength(1);
    expect(callbacks.onTerminalClose).toHaveBeenCalledWith({
      code: 1000,
      reason: "Replaced by a newer connection",
    });
    expect(callbacks.onReconnectScheduled).not.toHaveBeenCalled();
  });

  it("reconnects after a relay session expiry", () => {
    const { callbacks, connection, sockets } = setup();
    connection.start();

    sockets[0].serverClose(1000, "Session expired");
    vi.advanceTimersByTime(1_000);

    expect(sockets).toHaveLength(2);
    expect(callbacks.onTerminalClose).not.toHaveBeenCalled();
  });

  it("does not queue or replay messages sent while disconnected", () => {
    const { connection, sockets } = setup();
    connection.start();
    sockets[0].open();
    expect(connection.send("before disconnect")).toBe(true);

    sockets[0].serverClose(1006);
    expect(connection.send("during disconnect")).toBe(false);
    vi.advanceTimersByTime(1_000);
    sockets[1].open();

    expect(sockets[0].sent).toEqual(["before disconnect"]);
    expect(sockets[1].sent).toEqual([]);
  });

  it("clears retries and ignores stale socket events when stopped", () => {
    const { callbacks, connection, sockets } = setup();
    connection.start();
    sockets[0].serverClose(1006);
    sockets[0].message("late");

    connection.stop();
    vi.advanceTimersByTime(60_000);

    expect(sockets).toHaveLength(1);
    expect(callbacks.onMessage).not.toHaveBeenCalled();
    expect(connection.send("after stop")).toBe(false);
  });

  it("closes an active socket when stopped", () => {
    const { callbacks, connection, sockets } = setup();
    connection.start();
    sockets[0].open();

    connection.stop();

    expect(sockets[0].closeCalls).toBe(1);
    expect(callbacks.onTerminalClose).not.toHaveBeenCalled();
    expect(callbacks.onReconnectScheduled).not.toHaveBeenCalled();
  });
});
