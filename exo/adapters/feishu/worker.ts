// Feishu (Lark) adapter worker.
//
// Built on the official Lark SDK:
//   - Lark.Client for outbound messages (im.message.create)
//   - Lark.WSClient for inbound messages over the open platform's
//     long-connection mode, so no public callback URL is required. The
//     worker runs fine on a laptop behind NAT, which matches how exo is
//     usually deployed.
//
// Required app permissions (Feishu/Lark open platform console):
//   - im:message:send_as_bot
//   - im:message.group_at_msg and/or im:message.p2p_msg
// Event subscriptions must use "long connection" mode and subscribe to
// `im.message.receive_v1`. See setup-prompt.md for the full walkthrough.

// This shim must run before the Lark SDK is imported. The SDK's internal
// loggers (Client, WSClient, EventDispatcher) write `[info]: ...` lines
// straight to process.stdout even with loggerLevel configured, and the
// adapter runtime parses our stdout as a strict JSON event protocol — one
// stray line kills the worker and triggers a restart loop. Only JSON event
// lines pass through; everything else is redirected to stderr where it
// stays visible for debugging.
const originalStdoutWrite = process.stdout.write.bind(process.stdout);
process.stdout.write = ((chunk: unknown, ...args: unknown[]): boolean => {
  const text =
    typeof chunk === "string"
      ? chunk
      : Buffer.isBuffer(chunk)
        ? chunk.toString("utf8")
        : String(chunk);
  if (text.trimStart().startsWith("{")) {
    return originalStdoutWrite(
      chunk as Parameters<typeof process.stdout.write>[0],
      ...(args as never[]),
    );
  }
  return process.stderr.write(
    chunk as Parameters<typeof process.stderr.write>[0],
    ...(args as never[]),
  );
}) as typeof process.stdout.write;

import readline from "node:readline/promises";

import * as Lark from "@larksuiteoapi/node-sdk";

import {
  adapterConfig,
  optionalStringField,
  parseWorkerCommand,
  stringField,
  writeWorkerEvent,
} from "../protocol";
import {
  type FeishuTriggerPolicy,
  feishuSendUuid,
  parseInboundEvent,
  receiveIdTypeForTarget,
} from "./feishu";

const config = adapterConfig();

const appId = stringField(config, "appId");
const appSecretEnv =
  optionalStringField(config, "appSecretEnv") ?? "EXO_FEISHU_APP_SECRET";
const appSecret = requiredEnv(appSecretEnv, "Feishu app secret");

const domainName = optionalStringField(config, "domain") ?? "feishu";
if (domainName !== "feishu" && domainName !== "lark") {
  throw new Error("Feishu domain must be feishu or lark");
}
const domain = domainName === "lark" ? Lark.Domain.Lark : Lark.Domain.Feishu;

const trigger: FeishuTriggerPolicy = feishuTrigger(
  optionalStringField(config, "trigger") ?? "mentions_only",
);
const defaultTarget = optionalStringField(config, "defaultTarget");

process.on("unhandledRejection", (reason) => {
  reportWorkerError(
    `Feishu adapter unhandled rejection: ${
      reason instanceof Error ? reason.message : String(reason)
    }`,
  );
});

process.on("uncaughtException", (error) => {
  reportWorkerError(`Feishu adapter uncaught exception: ${error.message}`);
  process.exit(1);
});

// The SDK loggers still get loggerLevel as a second line of defense behind
// the stdout shim above; errors go to stderr where they stay debuggable.
const client = new Lark.Client({
  appId,
  appSecret,
  appType: Lark.AppType.SelfBuild,
  domain,
  disableTokenCache: false,
  loggerLevel: Lark.LoggerLevel.error,
});

const eventDispatcher = new Lark.EventDispatcher({}).register({
  "im.message.receive_v1": async (event) => {
    handleInboundMessage(event);
    return { code: 0 };
  },
});

// The SDK reports connection state through constructor callbacks: onReady
// fires on the first successful handshake, onError only when the client
// gives up for good (fatal config pull or exhausted retries), and the
// reconnect pair covers transient drops. `connected` is emitted from
// onReady instead of optimistically at spawn time.
const wsClient = new Lark.WSClient({
  appId,
  appSecret,
  domain,
  loggerLevel: Lark.LoggerLevel.error,
  onReady: () => {
    writeWorkerEvent({
      type: "connected",
      subject: `feishu:${appId}`,
      metadata: { domain: domainName },
    });
  },
  onError: (error) => {
    writeWorkerEvent({
      type: "error",
      message: `Feishu long connection failed: ${error.message}`,
    });
    process.exit(1);
  },
  onReconnecting: () => {
    writeWorkerEvent({ type: "lifecycle", name: "reconnecting" });
  },
  onReconnected: () => {
    writeWorkerEvent({ type: "lifecycle", name: "reconnected" });
  },
});

writeWorkerEvent({
  type: "lifecycle",
  name: "starting",
  metadata: { domain: domainName, trigger },
});

void wsClient.start({ eventDispatcher });

const input = readline.createInterface({
  input: process.stdin,
  crlfDelay: Number.POSITIVE_INFINITY,
});

for await (const line of input) {
  if (line.trim().length === 0) {
    continue;
  }
  let commandId: string | null = null;
  try {
    const command = parseWorkerCommand(JSON.parse(line));
    commandId = command.id;
    const target = command.target ?? defaultTarget;
    if (target === null) {
      throw new Error(
        "Feishu send_message needs a target (oc_/ou_ id from the inbound wakeup) or a defaultTarget in the adapter config",
      );
    }
    if (command.attachments.length > 0) {
      throw new Error(
        "the Feishu adapter does not support attachments yet; send text only",
      );
    }
    await sendText(target, command.text, command.id);
    writeWorkerEvent({ type: "command_ack", command_id: command.id });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    reportWorkerError(message);
    if (commandId !== null) {
      writeWorkerEvent({
        type: "command_nack",
        command_id: commandId,
        message,
      });
    }
  }
}

// stdin EOF means the adapter runtime is gone; the open WS handle would
// otherwise keep the process alive as an orphan.
writeWorkerEvent({ type: "disconnected", reason: "stdin closed" });
process.exit(0);

function handleInboundMessage(event: unknown): void {
  const inbound = parseInboundEvent(event, trigger);
  if (inbound === null) {
    return;
  }
  writeWorkerEvent({
    type: "message",
    target: inbound.chatId,
    sender: inbound.sender,
    text: inbound.text,
    message_id: inbound.messageId,
    metadata: {
      chat_type: inbound.chatType,
      mentioned_bot: inbound.mentionedBot,
    },
  });
}

function feishuTrigger(value: string): FeishuTriggerPolicy {
  if (value !== "all_messages" && value !== "mentions_only") {
    throw new Error("Feishu trigger must be all_messages or mentions_only");
  }
  return value;
}

// Outbound delivery is at-least-once: a message claimed but not acked before
// the worker dies is requeued and sent again. The runtime keeps the same
// message id across those attempts, and Feishu dedupes by `uuid` for an hour,
// so passing the id through turns a redelivery into a no-op on their side
// instead of a second message in the chat.
async function sendText(
  target: string,
  text: string,
  deliveryId: string,
): Promise<void> {
  const response = await client.im.message.create({
    params: { receive_id_type: receiveIdTypeForTarget(target) },
    data: {
      receive_id: target,
      msg_type: "text",
      content: JSON.stringify({ text }),
      uuid: feishuSendUuid(deliveryId),
    },
  });
  if ((response.code ?? 0) !== 0) {
    throw new Error(
      `feishu send failed with code ${response.code}: ${response.msg ?? "unknown error"}`,
    );
  }
}

function requiredEnv(name: string, label: string): string {
  const value = process.env[name];
  if (!value || value.length === 0) {
    throw new Error(`${label} env ${name} must be set`);
  }
  return value;
}

function reportWorkerError(message: string): void {
  writeWorkerEvent({ type: "error", message });
}
