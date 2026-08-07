import fs from "node:fs/promises";
import readline from "node:readline/promises";

import makeWASocket, {
  DisconnectReason,
  fetchLatestBaileysVersion,
  useMultiFileAuthState,
  type WAMessage,
} from "@whiskeysockets/baileys";
import type { ILogger } from "@whiskeysockets/baileys/lib/Utils/logger.js";
import qrcodeTerminal from "qrcode-terminal";

import {
  type AdapterAttachment,
  adapterConfig,
  optionalStringField,
  parseWorkerCommand,
  writeWorkerEvent,
} from "../protocol";

const config = adapterConfig();
const trigger = optionalStringField(config, "trigger") ?? "all_messages";
const allowedChats = stringArrayOrNull(config.allowedChats);
if (trigger !== "all_messages" && trigger !== "contacts_only") {
  throw new Error("WhatsApp trigger must be all_messages or contacts_only");
}
const linkMethod = optionalStringField(config, "linkMethod") ?? "qr";
if (linkMethod !== "qr" && linkMethod !== "pairing-code") {
  throw new Error("WhatsApp linkMethod must be qr or pairing-code");
}
const pairingPhoneNumber = optionalStringField(config, "phoneNumber");
const authDir =
  optionalStringField(config, "authDir") ??
  (process.env.EXO_ADAPTER_STATE_DIR === undefined
    ? `.exo/adapters/whatsapp/${process.env.EXO_ADAPTER_ID ?? "default"}/auth`
    : `${process.env.EXO_ADAPTER_STATE_DIR}/auth`);

const logger: ILogger = {
  level: "silent",
  child() {
    return logger;
  },
  trace() {},
  debug() {},
  info() {},
  warn(value) {
    process.stderr.write(`[whatsapp-adapter] ${formatLogValue(value)}\n`);
  },
  error(value) {
    process.stderr.write(`[whatsapp-adapter] ${formatLogValue(value)}\n`);
  },
};

await fs.mkdir(authDir, { recursive: true });

const { state, saveCreds } = await useMultiFileAuthState(authDir);
const { version } = await fetchLatestBaileysVersion();
const SEND_TIMEOUT_MS = 60_000;
const socket = makeWASocket({
  auth: state,
  logger,
  printQRInTerminal: false,
  syncFullHistory: false,
  version,
});

socket.ev.on("creds.update", saveCreds);

socket.ev.on("connection.update", (update) => {
  if (update.qr && linkMethod === "qr") {
    qrcodeTerminal.generate(update.qr, { small: true }, (qr) => {
      process.stderr.write(
        `\n[whatsapp-adapter] Scan this QR with WhatsApp:\n${qr}\n`,
      );
    });
    writeWorkerEvent({
      type: "lifecycle",
      name: "qr",
      metadata: { qr: update.qr },
    });
  }
  if (update.connection === "open") {
    writeWorkerEvent({
      type: "connected",
      subject: socket.user?.id ?? null,
    });
  }
  if (update.connection === "close") {
    const statusCode = statusCodeFromError(update.lastDisconnect?.error);
    writeWorkerEvent({
      type: "disconnected",
      reason: statusCode === null ? null : String(statusCode),
    });
    if (statusCode === DisconnectReason.loggedOut) {
      writeWorkerEvent({
        type: "error",
        message: `WhatsApp session logged out; delete ${authDir} and pair again`,
      });
      process.exit(1);
    }
    process.exit(1);
  }
});

if (linkMethod === "pairing-code" && !state.creds.registered) {
  if (!pairingPhoneNumber) {
    throw new Error("WhatsApp pairing-code linkMethod requires phoneNumber");
  }
  const code = await socket.requestPairingCode(pairingPhoneNumber);
  process.stderr.write(
    `\n[whatsapp-adapter] Enter this WhatsApp pairing code for ${pairingPhoneNumber}: ${code}\n`,
  );
  writeWorkerEvent({
    type: "lifecycle",
    name: "pairing_code",
    metadata: { phoneNumber: pairingPhoneNumber, code },
  });
}

socket.ev.on("messages.upsert", (event) => {
  if (event.type !== "notify") {
    return;
  }
  for (const message of event.messages) {
    const inbound = inboundMessage(message);
    if (inbound === null) {
      continue;
    }
    if (!shouldTrigger(inbound.target)) {
      continue;
    }
    writeWorkerEvent(inbound);
  }
});

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
    if (!command.target) {
      throw new Error("WhatsApp send_message requires a target chat id");
    }
    writeWorkerEvent({
      type: "lifecycle",
      name: "send_starting",
      metadata: {
        target: command.target,
        attachmentCount: command.attachments.length,
      },
    });
    if (command.attachments.length === 0) {
      await sendWhatsAppMessage(command.target, { text: command.text });
    } else {
      let captionUsed = false;
      const textBeforeMedia = command.attachments.every(
        (attachment) => !attachmentSupportsCaption(attachment),
      );
      if (textBeforeMedia) {
        await sendWhatsAppMessage(command.target, { text: command.text });
        captionUsed = true;
      }
      for (const attachment of command.attachments) {
        const caption: string | null =
          !captionUsed && attachmentSupportsCaption(attachment)
            ? command.text
            : null;
        captionUsed ||= caption !== null;
        await sendAttachment(command.target, attachment, caption);
      }
      if (!captionUsed) {
        await sendWhatsAppMessage(command.target, { text: command.text });
      }
    }
    writeWorkerEvent({
      type: "lifecycle",
      name: "send_result",
      metadata: {
        target: command.target,
        attachmentCount: command.attachments.length,
      },
    });
    writeWorkerEvent({ type: "command_ack", command_id: command.id });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    writeWorkerEvent({
      type: "error",
      message,
    });
    if (commandId !== null) {
      writeWorkerEvent({
        type: "command_nack",
        command_id: commandId,
        message,
      });
    }
  }
}

function attachmentSupportsCaption(attachment: AdapterAttachment): boolean {
  return (
    attachment.kind === "image" ||
    attachment.kind === "video" ||
    attachment.kind === "document"
  );
}

async function sendAttachment(
  target: string,
  attachment: AdapterAttachment,
  caption: string | null,
): Promise<void> {
  await sendWhatsAppMessage(
    target,
    await whatsappAttachmentContent(attachment, caption),
  );
}

type WhatsAppMessageContent = Parameters<typeof socket.sendMessage>[1];

async function whatsappAttachmentContent(
  attachment: AdapterAttachment,
  caption: string | null,
): Promise<WhatsAppMessageContent> {
  const media = await mediaSource(attachment);
  switch (attachment.kind) {
    case "image":
      return {
        image: media,
        caption: caption ?? undefined,
      } as WhatsAppMessageContent;
    case "video":
      return {
        video: media,
        caption: caption ?? undefined,
      } as WhatsAppMessageContent;
    case "audio":
      if (!isOpusAudio(attachment)) {
        return audioDocumentContent(attachment, media);
      }
      return {
        audio: media,
        mimetype: whatsappAudioMimeType(attachment),
        ptt: isOpusAudio(attachment),
      } as WhatsAppMessageContent;
    case "document":
      if (!attachment.mimeType) {
        throw new Error("WhatsApp document attachment requires mimeType");
      }
      if (!attachment.fileName) {
        throw new Error("WhatsApp document attachment requires fileName");
      }
      return {
        document: media,
        mimetype: attachment.mimeType,
        fileName: attachment.fileName,
        caption: caption ?? undefined,
      } as WhatsAppMessageContent;
  }
}

async function sendWhatsAppMessage(
  target: string,
  content: WhatsAppMessageContent,
): Promise<void> {
  let timeout: NodeJS.Timeout | null = null;
  try {
    await Promise.race([
      socket.sendMessage(target, content),
      new Promise<never>((_, reject) => {
        timeout = setTimeout(() => {
          reject(
            new Error(
              `WhatsApp send_message timed out after ${SEND_TIMEOUT_MS}ms`,
            ),
          );
        }, SEND_TIMEOUT_MS);
      }),
    ]);
  } finally {
    if (timeout !== null) {
      clearTimeout(timeout);
    }
  }
}

function audioDocumentContent(
  attachment: AdapterAttachment,
  media: { url: string } | Buffer,
): WhatsAppMessageContent {
  return {
    document: media,
    mimetype: attachment.mimeType ?? "application/octet-stream",
    fileName: attachment.fileName ?? "audio",
  } as WhatsAppMessageContent;
}

function whatsappAudioMimeType(
  attachment: AdapterAttachment,
): string | undefined {
  if (isOpusAudio(attachment)) {
    return "audio/ogg; codecs=opus";
  }
  return attachment.mimeType ?? undefined;
}

function isOpusAudio(attachment: AdapterAttachment): boolean {
  const mimeType = attachment.mimeType?.toLowerCase() ?? "";
  const fileName = attachment.fileName?.toLowerCase() ?? "";
  const source = (attachment.path ?? attachment.url ?? "").toLowerCase();
  return (
    mimeType.includes("opus") ||
    fileName.endsWith(".opus") ||
    source.endsWith(".opus")
  );
}

async function mediaSource(
  attachment: AdapterAttachment,
): Promise<{ url: string } | Buffer> {
  if (attachment.path) {
    return fs.readFile(attachment.path);
  }
  if (attachment.url) {
    return { url: attachment.url };
  }
  if (attachment.data) {
    return Buffer.from(base64Payload(attachment.data), "base64");
  }
  throw new Error("WhatsApp attachment requires path, url, or data");
}

function base64Payload(data: string): string {
  const dataUrlSeparator = data.indexOf(",");
  if (data.startsWith("data:") && dataUrlSeparator !== -1) {
    return data.slice(dataUrlSeparator + 1);
  }
  return data;
}

function inboundMessage(message: WAMessage) {
  if (message.key.fromMe) {
    return null;
  }
  const chatId = message.key.remoteJid;
  if (!chatId) {
    return null;
  }
  const text = messageText(message);
  if (text === null || text.length === 0) {
    return null;
  }
  return {
    type: "message" as const,
    target: chatId,
    sender: message.key.participant ?? message.key.remoteJid ?? null,
    text,
    message_id: message.key.id ?? null,
  };
}

function messageText(message: WAMessage): string | null {
  const content = message.message;
  if (!content) {
    return null;
  }
  if (content.conversation) {
    return content.conversation;
  }
  if (content.extendedTextMessage?.text) {
    return content.extendedTextMessage.text;
  }
  if (content.imageMessage?.caption) {
    return content.imageMessage.caption;
  }
  if (content.videoMessage?.caption) {
    return content.videoMessage.caption;
  }
  return null;
}

function shouldTrigger(chatId: string): boolean {
  if (allowedChats !== null && !allowedChats.includes(chatId)) {
    return false;
  }
  if (trigger === "contacts_only") {
    return !chatId.endsWith("@g.us");
  }
  return true;
}

function stringArrayOrNull(value: unknown): string[] | null {
  if (value === null || value === undefined) {
    return null;
  }
  if (Array.isArray(value) && value.every((item) => typeof item === "string")) {
    return value;
  }
  throw new Error("WhatsApp allowedChats must be null or an array of strings");
}

function statusCodeFromError(error: unknown): number | null {
  if (!isRecord(error)) {
    return null;
  }
  const output = error.output;
  if (!isRecord(output) || typeof output.statusCode !== "number") {
    return null;
  }
  return output.statusCode;
}

function formatLogValue(value: unknown): string {
  if (value instanceof Error) {
    return value.stack ?? value.message;
  }
  if (typeof value === "string") {
    return value;
  }
  return JSON.stringify(value);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
