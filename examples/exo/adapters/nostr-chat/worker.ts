import fs from "node:fs/promises";
import path from "node:path";
import readline from "node:readline/promises";

import {
  finalizeEvent,
  generateSecretKey,
  verifyEvent,
  type Event,
} from "nostr-tools/pure";
import { Relay } from "nostr-tools/relay";

import {
  adapterConfig,
  optionalStringField,
  parseWorkerCommand,
  stringField,
  writeWorkerEvent,
} from "../protocol";
import {
  createChatEvent,
  isGroupChatEvent,
  normalizeRelayUrl,
  parseSecretKey,
  publicIdentity,
  relayInformationUrl,
  selectPreviousPrefixes,
  shouldTrigger,
  type NostrChatTrigger,
} from "./nostr-chat";

const config = adapterConfig();
const relayUrl = normalizeRelayUrl(
  optionalStringField(config, "relayUrl") ?? "wss://relay.openagents.com",
);
const groupId = optionalStringField(config, "groupId") ?? "openagents-public";
const trigger = stringField(config, "trigger") as NostrChatTrigger;
if (trigger !== "all_messages" && trigger !== "mentions_only") {
  throw new Error("Nostr chat trigger must be all_messages or mentions_only");
}

const stateDir =
  process.env.EXO_ADAPTER_STATE_DIR ??
  `.exo/adapters/nostr-chat/${process.env.EXO_ADAPTER_ID ?? "default"}`;
const secretKeyPath = path.join(stateDir, "secret.key");
const secretKey = await loadOrCreateSecretKey();
const identity = publicIdentity(secretKey);
const relaySelfPubkey = await fetchRelaySelfPubkey(relayUrl);
const relay = new Relay(relayUrl, {
  enablePing: true,
  enableReconnect: true,
});

relay.onauth = async (template) => finalizeEvent(template, secretKey);
relay.onnotice = (message) => {
  writeWorkerEvent({
    type: "lifecycle",
    name: "relay_notice",
    metadata: { message, relayUrl },
  });
};
relay.onclose = () => {
  writeWorkerEvent({
    type: "disconnected",
    reason: `Nostr relay connection closed: ${relayUrl}`,
  });
};
await relay.connect({ timeout: 10_000 });

const timeline: Event[] = [];
const seenEventIds = new Set<string>();
let historyCurrent = false;
let stateCurrent = false;
let connectedEmitted = false;

relay.subscribe([{ kinds: [9], "#h": [groupId], limit: 50 }], {
  onevent: handleChatEvent,
  oninvalidevent: () => {
    writeWorkerEvent({
      type: "error",
      message: "Nostr relay sent an event with an invalid signature",
    });
  },
  oneose: () => {
    historyCurrent = true;
    emitConnectedWhenCurrent();
  },
  onclose: (reason) => {
    writeWorkerEvent({
      type: "error",
      message: `Nostr chat subscription closed: ${reason}`,
    });
  },
});

relay.subscribe(
  [
    {
      kinds: [39000, 39001, 39003, 39005],
      authors: [relaySelfPubkey],
      "#d": [groupId],
    },
  ],
  {
    onevent: (event) => {
      if (!verifyEvent(event) || event.pubkey !== relaySelfPubkey) {
        writeWorkerEvent({
          type: "error",
          message:
            "Rejected NIP-29 group state that was not signed by the relay",
        });
      }
    },
    oninvalidevent: () => {
      writeWorkerEvent({
        type: "error",
        message: "Nostr relay sent invalid NIP-29 group state",
      });
    },
    oneose: () => {
      stateCurrent = true;
      emitConnectedWhenCurrent();
    },
    onclose: (reason) => {
      writeWorkerEvent({
        type: "error",
        message: `NIP-29 group-state subscription closed: ${reason}`,
      });
    },
  },
);

const input = readline.createInterface({
  input: process.stdin,
  crlfDelay: Number.POSITIVE_INFINITY,
});

input.on("error", (error) => {
  writeWorkerEvent({
    type: "error",
    message: `Nostr chat command stream error: ${error.message}`,
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
    const target = command.target ?? groupId;
    if (target !== groupId) {
      throw new Error(
        `Nostr chat target must be null or the configured group id ${groupId}`,
      );
    }
    if (command.attachments.length > 0) {
      throw new Error(
        "Nostr chat attachments are not available yet; send public HTTPS media URLs in the message text",
      );
    }
    const event = createChatEvent(
      command.text,
      groupId,
      selectPreviousPrefixes(timeline, identity.pubkey),
      secretKey,
    );
    writeWorkerEvent({
      type: "lifecycle",
      name: "send_starting",
      metadata: { eventId: event.id, groupId, relayUrl },
    });
    await publishWithAuthRetry(event);
    rememberTimelineEvent(event);
    writeWorkerEvent({
      type: "lifecycle",
      name: "send_result",
      metadata: { eventId: event.id, groupId, relayUrl },
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

function handleChatEvent(event: Event): void {
  if (!isGroupChatEvent(event, groupId)) {
    writeWorkerEvent({
      type: "error",
      message: "Rejected an invalid NIP-29 chat event",
    });
    return;
  }
  if (seenEventIds.has(event.id)) {
    return;
  }
  seenEventIds.add(event.id);
  rememberTimelineEvent(event);
  if (!historyCurrent || !shouldTrigger(trigger, event, identity.pubkey)) {
    return;
  }
  writeWorkerEvent({
    type: "message",
    target: groupId,
    sender: event.pubkey,
    text: event.content,
    message_id: event.id,
    metadata: {
      createdAt: event.created_at,
      eventId: event.id,
      groupId,
      kind: event.kind,
      relayUrl,
      source: "nostr-chat",
    },
  });
}

function rememberTimelineEvent(event: Event): void {
  if (!timeline.some((candidate) => candidate.id === event.id)) {
    timeline.push(event);
  }
  timeline.sort(
    (left, right) =>
      right.created_at - left.created_at || right.id.localeCompare(left.id),
  );
  timeline.splice(50);
  if (seenEventIds.size > 1_000) {
    const retained = new Set(timeline.map((candidate) => candidate.id));
    for (const eventId of seenEventIds) {
      if (!retained.has(eventId)) {
        seenEventIds.delete(eventId);
      }
    }
  }
}

function emitConnectedWhenCurrent(): void {
  if (connectedEmitted || !historyCurrent || !stateCurrent) {
    return;
  }
  connectedEmitted = true;
  process.stderr.write(
    `[nostr-chat-adapter] connected ${relayUrl}#${groupId} as ${identity.npub}\n`,
  );
  writeWorkerEvent({
    type: "connected",
    subject: `${relayUrl}#${groupId}`,
    metadata: {
      groupId,
      npub: identity.npub,
      pubkey: identity.pubkey,
      relaySelfPubkey,
      relayUrl,
    },
  });
}

async function publishWithAuthRetry(event: Event): Promise<void> {
  try {
    await relay.publish(event);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (!message.startsWith("auth-required:")) {
      throw error;
    }
    if (!relay.onauth) {
      throw new Error("Nostr relay requested authentication without a signer");
    }
    await relay.auth(relay.onauth);
    await relay.publish(event);
  }
}

async function loadOrCreateSecretKey(): Promise<Uint8Array> {
  const configured = process.env.EXO_NOSTR_SECRET_KEY;
  if (configured) {
    return parseSecretKey(configured);
  }
  await fs.mkdir(stateDir, { recursive: true, mode: 0o700 });
  try {
    return parseSecretKey(await fs.readFile(secretKeyPath, "utf8"));
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
      throw error;
    }
  }
  const generated = generateSecretKey();
  await fs.writeFile(
    secretKeyPath,
    `${Buffer.from(generated).toString("hex")}\n`,
    { mode: 0o600 },
  );
  return generated;
}

async function fetchRelaySelfPubkey(url: string): Promise<string> {
  const response = await fetch(relayInformationUrl(url), {
    headers: { accept: "application/nostr+json" },
    signal: AbortSignal.timeout(10_000),
  });
  if (!response.ok) {
    throw new Error(
      `Nostr relay information request failed with HTTP ${response.status}`,
    );
  }
  const document = (await response.json()) as unknown;
  if (
    !isRecord(document) ||
    typeof document.self !== "string" ||
    !/^[0-9a-f]{64}$/i.test(document.self)
  ) {
    throw new Error(
      "Nostr relay information does not contain a valid self key",
    );
  }
  return document.self.toLowerCase();
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
