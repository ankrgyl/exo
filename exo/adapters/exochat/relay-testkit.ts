// Test-only host for the real ExoChat relay: RendezvousSession from
// website/src/worker.js served over a minimal RFC 6455 server, so the real
// adapter process and a real peer client exercise the deployed relay code
// end-to-end. The durable-object runtime is shimmed (sockets survive an
// instance swap, storage is a map), which is exactly the hibernation contract
// the replay cache is written against.
import { createHash } from "node:crypto";
import http from "node:http";
import type { Duplex } from "node:stream";

import type {
  RelayContext,
  RelayEnv,
  RelaySocket,
} from "../../../website/src/worker.js";
import { RendezvousSession } from "../../../website/src/worker.js";

const WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const WS_PATH_PATTERN = /^\/chat\/ws\/([A-Za-z0-9_-]{8,128})\/?$/;

class MemoryStorage {
  values = new Map<string, unknown>();

  async get(key: string): Promise<unknown> {
    return this.values.get(key);
  }

  async put(key: string, value: unknown): Promise<void> {
    this.values.set(key, value);
  }

  async setAlarm(): Promise<void> {}

  async deleteAll(): Promise<void> {
    this.values.clear();
  }
}

type HostedSocket = RelaySocket & {
  attachment: { role?: string } | undefined;
};

type Channel = {
  context: RelayContext;
  session: RendezvousSession;
  sockets: Set<HostedSocket>;
  storage: MemoryStorage;
  ackDropsLeft: number;
  droppedAcks: string[];
  ackDropWaiters: (() => void)[];
};

export type RelayHarness = {
  url: string;
  dropAcks(channelId: string, count: number): void;
  waitForDroppedAck(channelId: string): Promise<void>;
  restartChannel(
    channelId: string,
    options: { clearStorage: boolean },
  ): Promise<void>;
  close(): Promise<void>;
};

export async function startRelayHarness(env?: RelayEnv): Promise<RelayHarness> {
  const channels = new Map<string, Channel>();

  function channelFor(channelId: string): Channel {
    const existing = channels.get(channelId);
    if (existing) {
      return existing;
    }
    const sockets = new Set<HostedSocket>();
    const storage = new MemoryStorage();
    const context: RelayContext = {
      getWebSockets: () => [...sockets],
      acceptWebSocket: (socket) => {
        sockets.add(socket as HostedSocket);
      },
      storage,
    };
    const channel: Channel = {
      context,
      session: new RendezvousSession(context, env),
      sockets,
      storage,
      ackDropsLeft: 0,
      droppedAcks: [],
      ackDropWaiters: [],
    };
    channels.set(channelId, channel);
    return channel;
  }

  const server = http.createServer((_request, response) => {
    response.writeHead(426).end();
  });
  // Upgraded sockets leave the http server's connection tracking, so keep our
  // own set or close() waits on them forever.
  const tcpSockets = new Set<Duplex>();

  server.on("upgrade", (request, tcpSocket) => {
    tcpSockets.add(tcpSocket);
    tcpSocket.on("close", () => {
      tcpSockets.delete(tcpSocket);
    });
    const url = new URL(request.url ?? "/", "http://localhost");
    const match = url.pathname.match(WS_PATH_PATTERN);
    const role = url.searchParams.get("role");
    const key = request.headers["sec-websocket-key"];
    const channelId = match?.[1];
    if (
      channelId === undefined ||
      (role !== "agent" && role !== "user") ||
      typeof key !== "string"
    ) {
      tcpSocket.destroy();
      return;
    }
    const accept = createHash("sha1")
      .update(`${key}${WS_GUID}`)
      .digest("base64");
    tcpSocket.write(
      "HTTP/1.1 101 Switching Protocols\r\n" +
        "Upgrade: websocket\r\n" +
        "Connection: Upgrade\r\n" +
        `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
    );
    attachConnection(channelFor(channelId), role, tcpSocket);
  });

  function attachConnection(
    channel: Channel,
    role: "agent" | "user",
    tcpSocket: Duplex,
  ): void {
    const socket: HostedSocket = {
      attachment: undefined,
      readyState: WebSocket.OPEN,
      send(message: string) {
        if (
          channel.ackDropsLeft > 0 &&
          socket.attachment?.role === "agent" &&
          isAck(message)
        ) {
          channel.ackDropsLeft -= 1;
          channel.droppedAcks.push(message);
          for (const wake of channel.ackDropWaiters.splice(0)) {
            wake();
          }
          return;
        }
        tcpSocket.write(encodeTextFrame(message));
      },
      close(code = 1000, reason = "") {
        channel.sockets.delete(socket);
        tcpSocket.write(encodeCloseFrame(code, reason));
        tcpSocket.end();
      },
      serializeAttachment(value: unknown) {
        socket.attachment = value as { role?: string };
      },
      deserializeAttachment() {
        return socket.attachment;
      },
    };

    // The connection ceremony RendezvousSession.fetch performs before the
    // runtime takes over the socket.
    for (const existing of channel.context.getWebSockets()) {
      if (existing.deserializeAttachment()?.role === role) {
        existing.close(1000, "Replaced by a newer connection");
      }
    }
    channel.context.acceptWebSocket(socket);
    socket.serializeAttachment({ role, connectedAt: Date.now() });
    channel.session.broadcastPresence();

    const state = { buffer: Buffer.alloc(0) };
    let dispatching: Promise<void> = Promise.resolve();
    tcpSocket.on("data", (chunk: Buffer) => {
      state.buffer = Buffer.concat([state.buffer, chunk]);
      for (const frame of parseFrames(state)) {
        dispatching = dispatching
          .then(() => dispatchFrame(frame))
          .catch(() => {
            tcpSocket.destroy();
          });
      }
    });
    tcpSocket.on("close", () => {
      if (channel.sockets.delete(socket)) {
        channel.session.webSocketClose();
      }
    });
    tcpSocket.on("error", () => {
      tcpSocket.destroy();
    });

    async function dispatchFrame(frame: WsFrame): Promise<void> {
      if (frame.opcode === 0x1) {
        await channel.session.webSocketMessage(
          socket,
          frame.payload.toString("utf8"),
        );
        return;
      }
      if (frame.opcode === 0x8) {
        socket.close(1000, "");
        return;
      }
      if (frame.opcode === 0x9) {
        tcpSocket.write(encodePongFrame(frame.payload));
      }
    }
  }

  await new Promise<void>((resolve) => {
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (address === null || typeof address === "string") {
    throw new Error("relay harness did not bind a TCP port");
  }

  return {
    url: `http://127.0.0.1:${address.port}`,
    dropAcks(channelId, count) {
      channelFor(channelId).ackDropsLeft = count;
    },
    waitForDroppedAck(channelId) {
      const channel = channelFor(channelId);
      if (channel.droppedAcks.length > 0) {
        return Promise.resolve();
      }
      return new Promise((resolve) => {
        channel.ackDropWaiters.push(resolve);
      });
    },
    async restartChannel(channelId, options) {
      const channel = channelFor(channelId);
      if (options.clearStorage) {
        await channel.storage.deleteAll();
      }
      channel.session = new RendezvousSession(channel.context, env);
    },
    close() {
      return new Promise((resolve) => {
        for (const tcpSocket of tcpSockets) {
          tcpSocket.destroy();
        }
        server.close(() => resolve());
        server.closeAllConnections();
      });
    },
  };
}

function isAck(message: string): boolean {
  try {
    const value = JSON.parse(message) as { channel?: string };
    return value.channel === "exo.chat.ack";
  } catch {
    return false;
  }
}

type WsFrame = {
  opcode: number;
  payload: Buffer;
};

function parseFrames(state: { buffer: Buffer }): WsFrame[] {
  const frames: WsFrame[] = [];
  for (;;) {
    const buffer = state.buffer;
    if (buffer.length < 2) {
      break;
    }
    const fin = (buffer[0] & 0x80) !== 0;
    const opcode = buffer[0] & 0x0f;
    const masked = (buffer[1] & 0x80) !== 0;
    let length = buffer[1] & 0x7f;
    let offset = 2;
    if (length === 126) {
      if (buffer.length < 4) {
        break;
      }
      length = buffer.readUInt16BE(2);
      offset = 4;
    } else if (length === 127) {
      if (buffer.length < 10) {
        break;
      }
      length = Number(buffer.readBigUInt64BE(2));
      offset = 10;
    }
    const maskOffset = offset;
    if (masked) {
      offset += 4;
    }
    if (buffer.length < offset + length) {
      break;
    }
    if (!fin || opcode === 0x0) {
      throw new Error("fragmented frames are unsupported by the test relay");
    }
    let payload = Buffer.from(buffer.subarray(offset, offset + length));
    if (masked) {
      const mask = buffer.subarray(maskOffset, maskOffset + 4);
      for (let index = 0; index < payload.length; index += 1) {
        payload[index] ^= mask[index % 4];
      }
    }
    state.buffer = buffer.subarray(offset + length);
    frames.push({ opcode, payload });
  }
  return frames;
}

function encodeTextFrame(text: string): Buffer {
  return encodeFrame(0x81, Buffer.from(text, "utf8"));
}

function encodePongFrame(payload: Buffer): Buffer {
  return encodeFrame(0x8a, payload);
}

function encodeCloseFrame(code: number, reason: string): Buffer {
  const reasonBytes = Buffer.from(reason, "utf8");
  const payload = Buffer.alloc(2 + reasonBytes.length);
  payload.writeUInt16BE(code, 0);
  reasonBytes.copy(payload, 2);
  return encodeFrame(0x88, payload);
}

function encodeFrame(firstByte: number, payload: Buffer): Buffer {
  if (payload.length < 126) {
    return Buffer.concat([Buffer.from([firstByte, payload.length]), payload]);
  }
  if (payload.length < 65_536) {
    const header = Buffer.alloc(4);
    header[0] = firstByte;
    header[1] = 126;
    header.writeUInt16BE(payload.length, 2);
    return Buffer.concat([header, payload]);
  }
  const header = Buffer.alloc(10);
  header[0] = firstByte;
  header[1] = 127;
  header.writeBigUInt64BE(BigInt(payload.length), 2);
  return Buffer.concat([header, payload]);
}
