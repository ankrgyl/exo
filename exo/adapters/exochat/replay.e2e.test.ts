// End-to-end replay-protection tests: the real adapter worker as a child
// process, the real relay class over real WebSockets, and a peer client that
// speaks the pre-id wire protocol so every assertion doubles as a
// compatibility check.
import { type ChildProcess, spawn } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs/promises";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import { startRelayHarness } from "./relay-testkit";

const TEST_TIMEOUT_MS = 30_000;
const WAIT_TIMEOUT_MS = 15_000;

const require = createRequire(import.meta.url);
const tsxCli = path.join(
  path.dirname(require.resolve("tsx/package.json")),
  "dist/cli.mjs",
);
const workerPath = fileURLToPath(new URL("./worker.ts", import.meta.url));

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitUntil<T>(get: () => T | null, what: string): Promise<T> {
  const deadline = Date.now() + WAIT_TIMEOUT_MS;
  for (;;) {
    const value = get();
    if (value !== null) {
      return value;
    }
    if (Date.now() > deadline) {
      throw new Error(`timed out waiting for ${what}`);
    }
    await sleep(25);
  }
}

class AdapterProcess {
  readonly events: Record<string, unknown>[] = [];
  private readonly child: ChildProcess;
  private stdoutRest = "";

  private constructor(child: ChildProcess) {
    this.child = child;
    child.stdout?.setEncoding("utf8");
    child.stdout?.on("data", (chunk: string) => {
      this.stdoutRest += chunk;
      const lines = this.stdoutRest.split("\n");
      this.stdoutRest = lines.pop() ?? "";
      for (const line of lines) {
        if (line.trim().length > 0) {
          this.events.push(JSON.parse(line) as Record<string, unknown>);
        }
      }
    });
    child.stderr?.resume();
  }

  static async start(options: {
    baseUrl: string;
    channelId: string;
    secret: string;
    stateDir: string;
  }): Promise<AdapterProcess> {
    const child = spawn(process.execPath, [tsxCli, workerPath], {
      env: {
        ...process.env,
        EXO_ADAPTER_CONFIG: JSON.stringify({
          baseUrl: options.baseUrl,
          channelId: options.channelId,
          secret: options.secret,
        }),
        EXO_ADAPTER_STATE_DIR: options.stateDir,
      },
      stdio: ["pipe", "pipe", "pipe"],
    });
    const adapter = new AdapterProcess(child);
    await adapter.waitForEvent((event) => event.type === "connected");
    return adapter;
  }

  writeCommand(command: Record<string, unknown>): void {
    this.child.stdin?.write(`${JSON.stringify(command)}\n`);
  }

  waitForEvent(
    predicate: (event: Record<string, unknown>) => boolean,
  ): Promise<Record<string, unknown>> {
    let cursor = 0;
    return waitUntil(() => {
      while (cursor < this.events.length) {
        const event = this.events[cursor];
        cursor += 1;
        if (predicate(event)) {
          return event;
        }
      }
      return null;
    }, "adapter event");
  }

  countEvents(type: string): number {
    return this.events.filter((event) => event.type === type).length;
  }

  kill(): void {
    this.child.kill("SIGKILL");
  }
}

// A peer that speaks the wire protocol exactly as it existed before replay
// protection: fixed-field AAD, no envelope id, no knowledge of acks. If this
// peer can authenticate and decrypt what the new adapter sends, an undeployed
// web client can too.
class UserPeer {
  readonly chats: { text: string; envelope: Record<string, unknown> }[] = [];
  readonly acks: Record<string, unknown>[] = [];
  private readonly ws: WebSocket;
  private readonly key: Buffer;
  private readonly channelId: string;
  private seq = 0;

  private constructor(ws: WebSocket, channelId: string, secret: string) {
    this.ws = ws;
    this.channelId = channelId;
    this.key = Buffer.from(
      crypto.hkdfSync(
        "sha256",
        Buffer.from(secret, "base64url"),
        Buffer.from(channelId),
        Buffer.from("exo-chat-relay:aes-gcm:v1"),
        32,
      ),
    );
    ws.addEventListener("message", (event) => {
      if (typeof event.data !== "string") {
        return;
      }
      const message = JSON.parse(event.data) as Record<string, unknown>;
      if (message.channel === "exo.chat.ack") {
        this.acks.push(message);
        return;
      }
      if (message.channel !== "exo.chat" || message.from !== "agent") {
        return;
      }
      const frame = this.decrypt(message);
      if (frame !== null && frame.type === "chat") {
        this.chats.push({ text: String(frame.text), envelope: message });
      }
    });
  }

  static async connect(
    baseUrl: string,
    channelId: string,
    secret: string,
  ): Promise<UserPeer> {
    const wsUrl = new URL(`/chat/ws/${channelId}`, baseUrl);
    wsUrl.protocol = "ws:";
    wsUrl.searchParams.set("role", "user");
    const ws = new WebSocket(wsUrl);
    await new Promise<void>((resolve, reject) => {
      ws.addEventListener("open", () => resolve(), { once: true });
      ws.addEventListener(
        "error",
        () => reject(new Error("peer failed to connect")),
        { once: true },
      );
    });
    return new UserPeer(ws, channelId, secret);
  }

  sendLegacyChat(id: string, text: string): void {
    const envelope: Record<string, unknown> = {
      channel: "exo.chat",
      channelId: this.channelId,
      ciphertext: "",
      from: "user",
      nonce: crypto.randomBytes(12).toString("base64url"),
      seq: (this.seq += 1),
      version: 1,
    };
    const cipher = crypto.createCipheriv(
      "aes-256-gcm",
      this.key,
      Buffer.from(String(envelope.nonce), "base64url"),
    );
    cipher.setAAD(Buffer.from(canonicalEnvelope(envelope)));
    const ciphertext = Buffer.concat([
      cipher.update(
        JSON.stringify({ type: "chat", id, text, createdAt: Date.now() }),
        "utf8",
      ),
      cipher.final(),
      cipher.getAuthTag(),
    ]);
    envelope.ciphertext = ciphertext.toString("base64url");
    this.ws.send(JSON.stringify(envelope));
  }

  waitForChats(count: number): Promise<void> {
    return waitUntil(
      () => (this.chats.length >= count ? true : null),
      `${count} chat broadcasts`,
    ).then(() => {});
  }

  close(): void {
    this.ws.close();
  }

  private decrypt(
    envelope: Record<string, unknown>,
  ): Record<string, unknown> | null {
    try {
      const bytes = Buffer.from(String(envelope.ciphertext), "base64url");
      const ciphertext = bytes.subarray(0, bytes.length - 16);
      const authTag = bytes.subarray(bytes.length - 16);
      const decipher = crypto.createDecipheriv(
        "aes-256-gcm",
        this.key,
        Buffer.from(String(envelope.nonce), "base64url"),
      );
      decipher.setAAD(Buffer.from(canonicalEnvelope(envelope)));
      decipher.setAuthTag(authTag);
      const plaintext = Buffer.concat([
        decipher.update(ciphertext),
        decipher.final(),
      ]).toString("utf8");
      return JSON.parse(plaintext) as Record<string, unknown>;
    } catch {
      return null;
    }
  }
}

// The pre-id canonical form: any envelope field outside this list — the
// dedupe id included — must stay outside the AAD or existing peers would fail
// authentication.
function canonicalEnvelope(envelope: Record<string, unknown>): string {
  return JSON.stringify({
    channel: envelope.channel,
    channelId: envelope.channelId,
    from: envelope.from,
    nonce: envelope.nonce,
    seq: envelope.seq,
    version: envelope.version,
  });
}

async function freshStateDir(): Promise<string> {
  return fs.mkdtemp(path.join(os.tmpdir(), "exochat-replay-e2e-"));
}

function testSecret(): string {
  return crypto.randomBytes(32).toString("base64url");
}

const sendCommand = {
  type: "send_message",
  id: "0198c4f1-7b2a-7c3d-8e4f-5a6b7c8d9e0f",
  text: "exactly once, please",
};

describe("exochat replay protection", () => {
  it(
    "replays after a crash between send and ack without a second broadcast",
    { timeout: TEST_TIMEOUT_MS },
    async () => {
      const harness = await startRelayHarness();
      const channelId = "replay-crash-channel";
      const secret = testSecret();
      const stateDir = await freshStateDir();
      let adapter: AdapterProcess | null = null;
      const peer = await UserPeer.connect(harness.url, channelId, secret);
      try {
        adapter = await AdapterProcess.start({
          baseUrl: harness.url,
          channelId,
          secret,
          stateDir,
        });

        // First attempt: the relay accepts and broadcasts, but the ack never
        // reaches the adapter — the crash window this design exists for.
        harness.dropAcks(channelId, 1);
        adapter.writeCommand(sendCommand);
        await harness.waitForDroppedAck(channelId);
        await peer.waitForChats(1);
        expect(adapter.countEvents("command_ack")).toBe(0);
        adapter.kill();

        // The restarted worker replays the same command id. The relay answers
        // with the original outcome instead of broadcasting again, and the
        // command still acks.
        adapter = await AdapterProcess.start({
          baseUrl: harness.url,
          channelId,
          secret,
          stateDir,
        });
        adapter.writeCommand(sendCommand);
        const ack = await adapter.waitForEvent(
          (event) => event.type === "command_ack",
        );
        expect(ack.command_id).toBe(sendCommand.id);

        await sleep(200);
        expect(peer.chats.length).toBe(1);
        expect(peer.chats[0].text).toBe(sendCommand.text);
        // The broadcast the old-protocol peer authenticated carried the new
        // envelope-level id.
        expect(typeof peer.chats[0].envelope.id).toBe("string");
      } finally {
        adapter?.kill();
        peer.close();
        await harness.close();
      }
    },
  );

  it(
    "re-broadcasts honestly after the replay window expires",
    { timeout: TEST_TIMEOUT_MS },
    async () => {
      const harness = await startRelayHarness({ EXOCHAT_REPLAY_TTL_MS: "200" });
      const channelId = "replay-expiry-channel";
      const secret = testSecret();
      const stateDir = await freshStateDir();
      let adapter: AdapterProcess | null = null;
      const peer = await UserPeer.connect(harness.url, channelId, secret);
      try {
        adapter = await AdapterProcess.start({
          baseUrl: harness.url,
          channelId,
          secret,
          stateDir,
        });

        adapter.writeCommand(sendCommand);
        await adapter.waitForEvent((event) => event.type === "command_ack");
        await sleep(400);
        adapter.writeCommand(sendCommand);
        await adapter.waitForEvent(
          (event) =>
            event.type === "command_ack" &&
            adapter !== null &&
            adapter.countEvents("command_ack") === 2,
        );
        await peer.waitForChats(2);
      } finally {
        adapter?.kill();
        peer.close();
        await harness.close();
      }
    },
  );

  it(
    "dedupes across a relay wake, and degrades to at-least-once when the cache is lost",
    { timeout: TEST_TIMEOUT_MS },
    async () => {
      const harness = await startRelayHarness();
      const channelId = "replay-restart-channel";
      const secret = testSecret();
      const stateDir = await freshStateDir();
      let adapter: AdapterProcess | null = null;
      const peer = await UserPeer.connect(harness.url, channelId, secret);
      try {
        adapter = await AdapterProcess.start({
          baseUrl: harness.url,
          channelId,
          secret,
          stateDir,
        });

        adapter.writeCommand(sendCommand);
        await adapter.waitForEvent((event) => event.type === "command_ack");
        await peer.waitForChats(1);

        // A hibernation wake: new instance, storage intact. The window holds.
        await harness.restartChannel(channelId, { clearStorage: false });
        adapter.writeCommand(sendCommand);
        await adapter.waitForEvent(
          (event) =>
            event.type === "command_ack" &&
            adapter !== null &&
            adapter.countEvents("command_ack") === 2,
        );
        await sleep(200);
        expect(peer.chats.length).toBe(1);

        // Cache loss: the send is accepted as new. Duplicate delivery, no
        // corruption — every broadcast still authenticates and decrypts.
        await harness.restartChannel(channelId, { clearStorage: true });
        adapter.writeCommand(sendCommand);
        await adapter.waitForEvent(
          (event) =>
            event.type === "command_ack" &&
            adapter !== null &&
            adapter.countEvents("command_ack") === 3,
        );
        await peer.waitForChats(2);
        expect(peer.chats[1].text).toBe(sendCommand.text);
      } finally {
        adapter?.kill();
        peer.close();
        await harness.close();
      }
    },
  );

  it(
    "relays id-less traffic from an old client untouched and never acks it",
    { timeout: TEST_TIMEOUT_MS },
    async () => {
      const harness = await startRelayHarness();
      const channelId = "replay-legacy-channel";
      const secret = testSecret();
      const stateDir = await freshStateDir();
      let adapter: AdapterProcess | null = null;
      const peer = await UserPeer.connect(harness.url, channelId, secret);
      try {
        adapter = await AdapterProcess.start({
          baseUrl: harness.url,
          channelId,
          secret,
          stateDir,
        });

        peer.sendLegacyChat("legacy-message-1", "hello from before ids");
        const message = await adapter.waitForEvent(
          (event) => event.type === "message",
        );
        expect(message.text).toBe("hello from before ids");
        expect(peer.acks.length).toBe(0);

        // Same id-less envelope again: the relay has no opinion, both copies
        // arrive — old clients keep exactly the semantics they had.
        peer.sendLegacyChat("legacy-message-1", "hello from before ids");
        await adapter.waitForEvent(
          (event) =>
            event.type === "message" &&
            adapter !== null &&
            adapter.countEvents("message") === 2,
        );
      } finally {
        adapter?.kill();
        peer.close();
        await harness.close();
      }
    },
  );
});
