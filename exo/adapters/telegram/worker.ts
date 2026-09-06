import {
  adapterConfig,
  optionalStringField,
  parseWorkerCommand,
  writeWorkerEvent,
} from "../protocol";

// Telegram adapter for Exo: long-polls the Bot API for inbound messages and
// speaks the standard worker protocol on stdio (see ../protocol.ts).

const config = adapterConfig();
const botTokenEnv =
  optionalStringField(config, "tokenEnv") ?? "EXO_TELEGRAM_BOT_TOKEN";
const botToken = requiredEnv(botTokenEnv, "Telegram bot token");
const defaultChatId = optionalStringField(config, "defaultChatId");
const trigger = optionalStringField(config, "trigger") ?? "all_messages";
const allowedChats = stringArrayOrNull(config.allowedChats);
const allowBots = config.allowBots === true;
if (trigger !== "all_messages" && trigger !== "mentions_only") {
  throw new Error("Telegram trigger must be all_messages or mentions_only");
}

const API = `https://api.telegram.org/bot${botToken}`;
const POLL_TIMEOUT_S = 25;
const SEND_TIMEOUT_MS = 30_000;
let offset = 0;

process.on("unhandledRejection", (reason) => {
  writeWorkerEvent({
    type: "error",
    message: `Telegram adapter unhandled rejection: ${
      reason instanceof Error ? reason.message : String(reason)
    }`,
  });
});
process.on("uncaughtException", (error) => {
  writeWorkerEvent({
    type: "error",
    message: `Telegram adapter crashed: ${error.message}`,
  });
  process.exit(1);
});

type TgUpdate = {
  update_id: number;
  message?: {
    message_id: number;
    chat: { id: number; type: string };
    from?: {
      id: number;
      is_bot: boolean;
      username?: string;
      first_name?: string;
    };
    text?: string;
    reply_to_message?: { message_id: number };
  };
};

async function tgApi(method: string, body: unknown): Promise<unknown> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), SEND_TIMEOUT_MS);
  try {
    const response = await fetch(`${API}/${method}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      signal: controller.signal,
    });
    const payload = (await response.json()) as {
      ok: boolean;
      result?: unknown;
      description?: string;
    };
    if (!payload.ok) throw new Error(payload.description ?? `${method} failed`);
    return payload.result;
  } finally {
    clearTimeout(timer);
  }
}

async function pollLoop(): Promise<void> {
  for (;;) {
    try {
      const result = (await tgApi("getUpdates", {
        timeout: POLL_TIMEOUT_S,
        offset,
        allowed_updates: ["message"],
      })) as TgUpdate[];
      for (const update of result) {
        offset = update.update_id + 1;
        const message = update.message;
        if (!message?.text) continue;
        const chat = message.chat;
        const chatId = String(chat.id);
        const from = message.from;
        if (!allowBots && (from?.is_bot ?? false)) continue;
        if (allowedChats && !allowedChats.includes(chatId)) continue;
        if (trigger === "mentions_only" && !message.reply_to_message) continue;
        writeWorkerEvent({
          type: "message",
          target: chatId,
          sender:
            from?.username ?? from?.first_name ?? String(from?.id ?? "unknown"),
          text: message.text,
          message_id: String(message.message_id),
          metadata: { chatType: chat.type },
        });
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      writeWorkerEvent({
        type: "error",
        message: `Telegram poll error: ${message}`,
      });
      await new Promise((resolve) => setTimeout(resolve, 5_000));
    }
  }
}

async function readStdin(): Promise<void> {
  const decoder = new TextDecoder();
  let buffer = "";
  for await (const chunk of process.stdin) {
    buffer += decoder.decode(chunk as Uint8Array, { stream: true });
    let newlineIndex: number;
    while ((newlineIndex = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, newlineIndex).trim();
      buffer = buffer.slice(newlineIndex + 1);
      if (!line) continue;
      let command: unknown;
      try {
        command = parseWorkerCommand(JSON.parse(line));
      } catch {
        writeWorkerEvent({
          type: "error",
          message: `Invalid worker command: ${line.slice(0, 100)}`,
        });
        continue;
      }
      const send = command as {
        id: string;
        target?: string | null;
        text: string;
      };
      const chatId = send.target ?? defaultChatId;
      if (!chatId) {
        writeWorkerEvent({
          type: "command_nack",
          command_id: send.id,
          message: "no target chat id",
        });
        continue;
      }
      try {
        await tgApi("sendMessage", { chat_id: chatId, text: send.text });
        writeWorkerEvent({ type: "command_ack", command_id: send.id });
      } catch (error) {
        writeWorkerEvent({
          type: "command_nack",
          command_id: send.id,
          message: error instanceof Error ? error.message : String(error),
        });
      }
    }
  }
}

function requiredEnv(name: string, what: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${what} is required (${name})`);
  return value;
}

function stringArrayOrNull(value: unknown): string[] | null {
  if (value == null) return null;
  if (!Array.isArray(value) || value.some((item) => typeof item !== "string")) {
    throw new Error("allowedChats must be an array of chat id strings or null");
  }
  return value as string[];
}

const me = (await tgApi("getMe", {})) as { username?: string };
writeWorkerEvent({ type: "connected", subject: me.username ?? "telegram" });
await Promise.all([pollLoop(), readStdin()]);
