import fs from "node:fs/promises";
import readline from "node:readline/promises";

import {
  ChannelType,
  Client,
  Events,
  GatewayIntentBits,
  Partials,
  type Message,
  type MessageCreateOptions,
} from "discord.js";

import {
  type AdapterAttachment,
  adapterConfig,
  optionalStringField,
  parseWorkerCommand,
  writeWorkerEvent,
} from "../protocol";
import {
  createResilienceHandlers,
  inboundAttachments,
  splitDiscordContent,
  startConnectionWatchdog,
} from "./discord";
import type { DiscordVoice as DiscordVoiceInstance } from "./voice";

const SEND_TIMEOUT_MS = 60_000;

const config = adapterConfig();
const tokenEnv =
  optionalStringField(config, "tokenEnv") ?? "EXO_DISCORD_BOT_TOKEN";
const token = process.env[tokenEnv];
if (!token) {
  throw new Error(`Discord bot token missing from ${tokenEnv}`);
}
const trigger = optionalStringField(config, "trigger") ?? "mentions_only";
const defaultChannelId = optionalStringField(config, "defaultChannelId");
const allowedChannels = stringArrayOrNull(config.allowedChannels);
const allowBots = config.allowBots === true;
const voiceEnabled = config.voice === true;
if (trigger !== "all_messages" && trigger !== "mentions_only") {
  throw new Error("Discord trigger must be all_messages or mentions_only");
}

const resilience = createResilienceHandlers({
  emit: writeWorkerEvent,
  exit: (code) => process.exit(code),
});

process.on("unhandledRejection", (reason) => {
  resilience.onUnhandledRejection(reason);
});

process.on("uncaughtException", (error) => {
  resilience.onUncaughtException(error);
});

const client = new Client({
  intents: [
    GatewayIntentBits.Guilds,
    GatewayIntentBits.GuildMessages,
    GatewayIntentBits.DirectMessages,
    GatewayIntentBits.MessageContent,
    // GuildVoiceStates (non-privileged) is needed to track who is in a voice
    // channel and to join it; added only when voice is enabled.
    ...(voiceEnabled ? [GatewayIntentBits.GuildVoiceStates] : []),
  ],
  partials: [Partials.Channel],
});

// Voice is a microphone and speaker on the text pipe: a spoken utterance
// becomes a normal `message` event (target = voice channel id); an outbound
// send to that target is spoken back. All audio stays in this worker.
let voice: DiscordVoiceInstance | null = null;
if (voiceEnabled) {
  const openaiKey = process.env.OPENAI_API_KEY;
  if (!openaiKey) {
    throw new Error(
      "Discord voice requires OPENAI_API_KEY in the worker environment",
    );
  }
  const { DiscordVoice } = await import("./voice");
  voice = new DiscordVoice(client, openaiKey, writeWorkerEvent);
  voice.register();
}

client.on("error", (error) => {
  reportWorkerError(error.message);
});

client.once(Events.ClientReady, () => {
  writeWorkerEvent({
    type: "connected",
    subject: client.user?.id ?? null,
    metadata: {
      username: client.user?.tag ?? null,
    },
  });
});

client.on("shardDisconnect", (event) => {
  resilience.onShardDisconnect(event.code);
});

startConnectionWatchdog({
  isReady: () => client.isReady(),
  emit: writeWorkerEvent,
  exit: (code) => process.exit(code),
});

client.on("shardError", (error) => {
  resilience.onShardError(error);
});

client.on("messageCreate", (message) => {
  if (message.author.id === client.user?.id) {
    return;
  }
  if (message.author.bot && !allowBots) {
    return;
  }
  if (!shouldTrigger(message)) {
    return;
  }
  writeWorkerEvent({
    type: "message",
    target: message.channelId,
    sender: message.author.id,
    text: message.content,
    message_id: message.id,
    metadata: {
      authorUsername: message.author.tag,
      channelId: message.channelId,
      guildId: message.guildId,
      channelType: message.channel.type,
    },
    attachments: inboundAttachments(message.attachments.values()),
  });
});

try {
  await client.login(token);
} catch (error) {
  resilience.onLoginFailure(error);
}

const input = readline.createInterface({
  input: process.stdin,
  crlfDelay: Number.POSITIVE_INFINITY,
});

input.on("error", (error) => {
  reportWorkerError(`Discord adapter command stream error: ${error.message}`);
});

try {
  for await (const line of input) {
    if (line.trim().length === 0) {
      continue;
    }
    let commandId: string | null = null;
    try {
      const command = parseWorkerCommand(JSON.parse(line));
      commandId = command.id;
      const target = command.target ?? defaultChannelId;
      if (!target) {
        throw new Error(
          "Discord send_message requires a target channel id or configured defaultChannelId",
        );
      }
      writeWorkerEvent({
        type: "lifecycle",
        name: "send_starting",
        metadata: {
          target,
          attachmentCount: command.attachments.length,
        },
      });
      const files = await discordAttachmentFiles(command.attachments);
      const contentChunks = splitDiscordContent(command.text);
      for (const [index, content] of contentChunks.entries()) {
        await sendDiscordMessage(target, {
          content,
          files: index === 0 ? files : [],
        });
      }
      // If this target has an active voice session, also speak the reply. The
      // text send above doubles as the inspectable transcript of the voice turn.
      if (voice) {
        await voice.maybeSpeak(target, command.text);
      }
      writeWorkerEvent({
        type: "lifecycle",
        name: "send_result",
        metadata: {
          target,
          attachmentCount: command.attachments.length,
        },
      });
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
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  reportWorkerError(
    `Discord adapter command stream closed with error: ${message}`,
  );
}

async function sendDiscordMessage(
  target: string,
  options: MessageCreateOptions,
): Promise<void> {
  const channel = await client.channels.fetch(target);
  if (!isSendableChannel(channel)) {
    throw new Error(`Discord target ${target} cannot send messages`);
  }
  let timeout: NodeJS.Timeout | null = null;
  try {
    await Promise.race([
      channel.send(options),
      new Promise<never>((_, reject) => {
        timeout = setTimeout(() => {
          reject(
            new Error(
              `Discord send_message timed out after ${SEND_TIMEOUT_MS}ms`,
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

type SendableChannel = {
  send(options: MessageCreateOptions): Promise<unknown>;
};

function isSendableChannel(channel: unknown): channel is SendableChannel {
  if (!channel || typeof channel !== "object" || !("send" in channel)) {
    return false;
  }
  return typeof (channel as { send?: unknown }).send === "function";
}

async function discordAttachmentFiles(
  attachments: AdapterAttachment[],
): Promise<NonNullable<MessageCreateOptions["files"]>> {
  return Promise.all(
    attachments.map(async (attachment) => ({
      attachment: await attachmentBytes(attachment),
      name: attachment.fileName ?? fileNameForAttachment(attachment),
      description: attachment.mimeType ?? undefined,
    })),
  );
}

async function attachmentBytes(attachment: AdapterAttachment): Promise<Buffer> {
  if (attachment.path) {
    return fs.readFile(attachment.path);
  }
  if (attachment.url) {
    const response = await fetch(attachment.url);
    if (!response.ok) {
      throw new Error(
        `Discord attachment URL fetch failed with ${response.status}`,
      );
    }
    return Buffer.from(await response.arrayBuffer());
  }
  if (attachment.data) {
    return Buffer.from(base64Payload(attachment.data), "base64");
  }
  throw new Error("Discord attachment requires path, url, or data");
}

function shouldTrigger(message: Message): boolean {
  if (
    allowedChannels !== null &&
    !allowedChannels.includes(message.channelId)
  ) {
    return false;
  }
  if (message.channel.type === ChannelType.DM) {
    return true;
  }
  const botId = client.user?.id;
  if (trigger === "all_messages") {
    return true;
  }
  return botId !== undefined && message.mentions.users.has(botId);
}

function fileNameForAttachment(attachment: AdapterAttachment): string {
  switch (attachment.kind) {
    case "image":
      return "image";
    case "video":
      return "video";
    case "audio":
      return "audio";
    case "document":
      return "document";
  }
}

function base64Payload(data: string): string {
  const dataUrlSeparator = data.indexOf(",");
  if (data.startsWith("data:") && dataUrlSeparator !== -1) {
    return data.slice(dataUrlSeparator + 1);
  }
  return data;
}

function reportWorkerError(message: string): void {
  writeWorkerEvent({ type: "error", message });
}

function stringArrayOrNull(value: unknown): string[] | null {
  if (value === undefined || value === null) {
    return null;
  }
  if (
    Array.isArray(value) &&
    value.every((item): item is string => typeof item === "string")
  ) {
    return value;
  }
  throw new Error(
    "Discord allowedChannels must be null or an array of strings",
  );
}
