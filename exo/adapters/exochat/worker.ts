import crypto from "node:crypto";
import fs from "node:fs/promises";
import path from "node:path";
import readline from "node:readline/promises";

import {
  adapterConfig,
  optionalStringField,
  parseWorkerCommand,
  writeWorkerEvent,
} from "../protocol";
import { parseSendAck, type SendAck, sendDedupeId } from "./exochat";

const config = adapterConfig();
const baseUrl = normalizeBaseUrl(
  optionalStringField(config, "baseUrl") ??
    process.env.EXO_CHAT_WS_BASE_URL ??
    process.env.EXO_CHAT_BASE_URL ??
    "https://exoharness.ai",
);
const configuredChannelId = optionalStringField(config, "channelId");
const configuredSecret =
  optionalStringField(config, "secret") ??
  process.env.EXO_EXOCHAT_SECRET?.trim() ??
  null;
const stateDir =
  process.env.EXO_ADAPTER_STATE_DIR ??
  `.exo/adapters/exochat/${process.env.EXO_ADAPTER_ID ?? "default"}`;
const sessionPath = path.join(stateDir, "session.json");
const role = "agent";
const SEND_TIMEOUT_MS = 30_000;

if ((configuredChannelId === null) !== (configuredSecret === null)) {
  throw new Error(
    "ExoChat channelId and secret must both be set or both be null",
  );
}

const session = await loadOrCreateSession();
const userUrl = sessionUrl("user", session.channelId, session.secret);
const agentUrl = sessionUrl("agent", session.channelId, session.secret);
let seq = 0;
let socket: WebSocket | null = null;
const pendingAcks = new Map<
  string,
  { resolve: (ack: SendAck) => void; reject: (error: Error) => void }
>();
// Declared before the top-level connect() call below runs.
let reconnectDelayMs = 1000;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

process.stderr.write(
  `\n[exochat-adapter] Open this ExoChat URL on your phone or browser:\n${userUrl}\n\n`,
);

await connect();

const input = readline.createInterface({
  input: process.stdin,
  crlfDelay: Number.POSITIVE_INFINITY,
});

input.on("error", (error) => {
  writeWorkerEvent({
    type: "error",
    message: `ExoChat command stream error: ${error.message}`,
  });
});

for await (const line of input) {
  if (line.trim().length === 0) {
    continue;
  }
  let commandId: string | null = null;
  try {
    const command = parseWorkerCommand(JSON.parse(line));
    commandId = command.id;
    const target = command.target ?? session.channelId;
    if (target !== session.channelId) {
      throw new Error(
        `ExoChat target must be null or the session channel id ${session.channelId}`,
      );
    }
    writeWorkerEvent({
      type: "lifecycle",
      name: "send_starting",
      metadata: { target },
    });
    if (command.attachments.length > 0) {
      throw new Error(
        "ExoChat is text-only right now; use another adapter for attachments",
      );
    }
    await sendFrame(
      {
        type: "chat",
        id: command.id,
        text: command.text,
        createdAt: Date.now(),
      },
      sendDedupeId(command.id),
    );
    writeWorkerEvent({
      type: "lifecycle",
      name: "send_result",
      metadata: { target },
    });
    writeWorkerEvent({ type: "command_ack", command_id: command.id });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    writeWorkerEvent({ type: "error", message });
    if (commandId !== null) {
      writeWorkerEvent({
        type: "command_nack",
        command_id: commandId,
        message,
      });
    }
  }
}

function scheduleReconnect(): void {
  if (reconnectTimer) {
    return;
  }
  const delay = reconnectDelayMs;
  reconnectDelayMs = Math.min(reconnectDelayMs * 2, 30_000);
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    void connect().catch(() => scheduleReconnect());
  }, delay);
}

async function connect(): Promise<void> {
  const wsUrl = new URL(`/chat/ws/${session.channelId}`, baseUrl);
  wsUrl.protocol = wsUrl.protocol === "https:" ? "wss:" : "ws:";
  wsUrl.searchParams.set("role", role);

  socket = new WebSocket(wsUrl);
  socket.addEventListener("message", (event) => {
    void handleSocketMessage(event.data).catch((error) => {
      const message = error instanceof Error ? error.message : String(error);
      writeWorkerEvent({ type: "error", message });
    });
  });
  socket.addEventListener("close", (event) => {
    writeWorkerEvent({
      type: "disconnected",
      reason: event.reason || String(event.code),
    });
    failPendingAcks("ExoChat WebSocket closed before the send was acked");
    // The relay drops idle connections; reconnect in-process instead of
    // exiting so the runner does not restart us (and reprint the chat URL).
    scheduleReconnect();
  });
  socket.addEventListener("error", () => {
    writeWorkerEvent({ type: "error", message: "ExoChat WebSocket error" });
  });

  await waitForOpen(socket);
  reconnectDelayMs = 1000;
  writeWorkerEvent({
    type: "connected",
    subject: session.channelId,
    metadata: { baseUrl, channelId: session.channelId, userUrl, agentUrl },
  });
  writeWorkerEvent({
    type: "lifecycle",
    name: "chat_url",
    metadata: { userUrl, agentUrl, channelId: session.channelId },
  });
  await sendFrame({
    type: "status",
    message: "Agent connected.",
    createdAt: Date.now(),
  });
}

function waitForOpen(ws: WebSocket): Promise<void> {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      reject(new Error("ExoChat WebSocket open timed out"));
    }, SEND_TIMEOUT_MS);
    ws.addEventListener(
      "open",
      () => {
        clearTimeout(timeout);
        resolve();
      },
      { once: true },
    );
    ws.addEventListener(
      "error",
      () => {
        clearTimeout(timeout);
        reject(new Error("ExoChat WebSocket failed to open"));
      },
      { once: true },
    );
  });
}

async function handleSocketMessage(data: unknown): Promise<void> {
  if (typeof data !== "string") {
    return;
  }
  const message = JSON.parse(data) as unknown;
  const ack = parseSendAck(message);
  if (ack) {
    pendingAcks.get(ack.id)?.resolve(ack);
    return;
  }
  if (isPresenceMessage(message)) {
    writeWorkerEvent({
      type: "lifecycle",
      name: "presence",
      metadata: { roles: message.roles },
    });
    return;
  }
  if (!isRelayEnvelope(message)) {
    return;
  }
  if (message.from === role) {
    return;
  }
  const frame = decryptRelayFrame(message);
  if (!frame) {
    writeWorkerEvent({
      type: "error",
      message: "Rejected ExoChat relay message that could not be decrypted",
    });
    return;
  }
  if (frame.type !== "chat") {
    return;
  }
  const text = typeof frame.text === "string" ? frame.text : "";
  if (text.length === 0) {
    return;
  }
  writeWorkerEvent({
    type: "message",
    target: session.channelId,
    sender: message.from,
    text,
    message_id:
      typeof frame.id === "string"
        ? frame.id
        : `${message.from}-${message.seq}`,
    metadata: {
      channelId: session.channelId,
      source: "exochat",
    },
  });
}

// A dedupe id opts the frame into the relay's replay protection: the id rides
// at envelope level (outside the ciphertext and outside the AAD, so pre-id
// peers still authenticate the envelope), and the send only settles once the
// relay acks the id. A duplicate ack is still success — the original broadcast
// already happened.
async function sendFrame(
  frame: Record<string, unknown>,
  dedupeId?: string,
): Promise<void> {
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    throw new Error("ExoChat WebSocket is not open");
  }
  const envelope = encryptRelayFrame(frame, ++seq);
  if (dedupeId !== undefined) {
    envelope.id = dedupeId;
  }
  const acked = dedupeId !== undefined ? waitForSendAck(dedupeId) : null;
  socket.send(JSON.stringify(envelope));
  if (acked) {
    await acked;
  }
}

function waitForSendAck(dedupeId: string): Promise<SendAck> {
  return new Promise<SendAck>((resolve, reject) => {
    const timeout = setTimeout(() => {
      pendingAcks.delete(dedupeId);
      reject(
        new Error(`ExoChat send was not acked within ${SEND_TIMEOUT_MS}ms`),
      );
    }, SEND_TIMEOUT_MS);
    pendingAcks.set(dedupeId, {
      resolve: (ack) => {
        clearTimeout(timeout);
        pendingAcks.delete(dedupeId);
        resolve(ack);
      },
      reject: (error) => {
        clearTimeout(timeout);
        pendingAcks.delete(dedupeId);
        reject(error);
      },
    });
  });
}

function failPendingAcks(message: string): void {
  for (const pending of pendingAcks.values()) {
    pending.reject(new Error(message));
  }
}

function encryptRelayFrame(
  frame: Record<string, unknown>,
  nextSeq: number,
): RelayEnvelope {
  const envelope: RelayEnvelope = {
    channel: "exo.chat",
    channelId: session.channelId,
    ciphertext: "",
    from: role,
    nonce: randomBase64url(12),
    seq: nextSeq,
    version: 1,
  };
  const cipher = crypto.createCipheriv(
    "aes-256-gcm",
    relayKey(),
    base64urlToBytes(envelope.nonce),
  );
  cipher.setAAD(Buffer.from(canonicalEnvelope(envelope)));
  const ciphertext = Buffer.concat([
    cipher.update(JSON.stringify(frame), "utf8"),
    cipher.final(),
    cipher.getAuthTag(),
  ]);
  envelope.ciphertext = ciphertext.toString("base64url");
  return envelope;
}

function decryptRelayFrame(
  envelope: RelayEnvelope,
): Record<string, unknown> | null {
  try {
    const bytes = base64urlToBytes(envelope.ciphertext);
    if (bytes.length < 17) {
      return null;
    }
    const ciphertext = bytes.subarray(0, bytes.length - 16);
    const authTag = bytes.subarray(bytes.length - 16);
    const decipher = crypto.createDecipheriv(
      "aes-256-gcm",
      relayKey(),
      base64urlToBytes(envelope.nonce),
    );
    decipher.setAAD(Buffer.from(canonicalEnvelope(envelope)));
    decipher.setAuthTag(authTag);
    const plaintext = Buffer.concat([
      decipher.update(ciphertext),
      decipher.final(),
    ]).toString("utf8");
    const frame = JSON.parse(plaintext) as unknown;
    return isRecord(frame) ? frame : null;
  } catch {
    return null;
  }
}

function relayKey(): Buffer {
  return Buffer.from(
    crypto.hkdfSync(
      "sha256",
      base64urlToBytes(session.secret),
      Buffer.from(session.channelId),
      Buffer.from("exo-chat-relay:aes-gcm:v1"),
      32,
    ),
  );
}

function canonicalEnvelope(envelope: RelayEnvelope): string {
  return JSON.stringify({
    channel: envelope.channel,
    channelId: envelope.channelId,
    from: envelope.from,
    nonce: envelope.nonce,
    seq: envelope.seq,
    version: envelope.version,
  });
}

async function loadOrCreateSession(): Promise<Session> {
  await fs.mkdir(stateDir, { recursive: true });
  if (configuredChannelId && configuredSecret) {
    const session = {
      channelId: configuredChannelId,
      secret: configuredSecret,
    };
    await writeSession(session);
    return session;
  }
  const existing = await readSession();
  if (existing) {
    return existing;
  }
  const session = {
    channelId: randomBase64url(18),
    secret: randomBase64url(32),
  };
  await writeSession(session);
  return session;
}

async function readSession(): Promise<Session | null> {
  try {
    const value = JSON.parse(await fs.readFile(sessionPath, "utf8")) as unknown;
    if (isSession(value)) {
      return value;
    }
    return null;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return null;
    }
    throw error;
  }
}

async function writeSession(session: Session): Promise<void> {
  await fs.writeFile(sessionPath, `${JSON.stringify(session, null, 2)}\n`, {
    mode: 0o600,
  });
}

function sessionUrl(
  sessionRole: "agent" | "user",
  channelId: string,
  secret: string,
): string {
  const url = new URL("/chat", baseUrl);
  url.searchParams.set("role", sessionRole);
  url.searchParams.set("c", channelId);
  url.hash = `k=${secret}`;
  return url.toString();
}

function normalizeBaseUrl(value: string): string {
  const url = new URL(value);
  url.hash = "";
  url.search = "";
  return url.toString().replace(/\/$/, "");
}

function randomBase64url(bytes: number): string {
  return crypto.randomBytes(bytes).toString("base64url");
}

function base64urlToBytes(value: string): Buffer {
  return Buffer.from(value, "base64url");
}

function isPresenceMessage(value: unknown): value is {
  channel: "rendezvous";
  type: "presence";
  roles: string[];
} {
  return (
    isRecord(value) &&
    value.channel === "rendezvous" &&
    value.type === "presence" &&
    Array.isArray(value.roles)
  );
}

function isRelayEnvelope(value: unknown): value is RelayEnvelope {
  return (
    isRecord(value) &&
    value.channel === "exo.chat" &&
    value.version === 1 &&
    value.channelId === session.channelId &&
    (value.from === "agent" || value.from === "user") &&
    typeof value.seq === "number" &&
    typeof value.nonce === "string" &&
    typeof value.ciphertext === "string"
  );
}

function isSession(value: unknown): value is Session {
  return (
    isRecord(value) &&
    typeof value.channelId === "string" &&
    value.channelId.length > 0 &&
    typeof value.secret === "string" &&
    value.secret.length > 0
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

type Session = {
  channelId: string;
  secret: string;
};

type RelayEnvelope = {
  channel: "exo.chat";
  channelId: string;
  ciphertext: string;
  from: "agent" | "user";
  id?: string;
  nonce: string;
  seq: number;
  version: 1;
};
