import type { JsonObject, ToolDefinition } from "./index";
import type { HarnessToolRegistry, ToolInstance } from "./tools";

export type AdapterToolName =
  | "create_adapter"
  | "list_adapters"
  | "enable_adapter"
  | "disable_adapter"
  | "delete_adapter"
  | "send_adapter_message";

export function registerAdapterTools(
  registry: HarnessToolRegistry,
  names: AdapterToolName[] = [
    "create_adapter",
    "list_adapters",
    "enable_adapter",
    "disable_adapter",
    "delete_adapter",
    "send_adapter_message",
  ],
): void {
  const requested = new Set<AdapterToolName>(names);
  for (const tool of createAdapterToolInstances()) {
    if (requested.has(tool.definition.name as AdapterToolName)) {
      registry.register(tool);
    }
  }
}

function createAdapterToolInstances(): ToolInstance[] {
  return [
    createAdapterTool(),
    listAdaptersTool(),
    enableAdapterTool(),
    disableAdapterTool(),
    deleteAdapterTool(),
    sendAdapterMessageTool(),
  ];
}

function createAdapterTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "create_adapter",
      description:
        "Create and enable a shipped long-running adapter for this conversation. Use source 'library'.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description:
              "Stable adapter name using letters, numbers, dashes, or underscores.",
          },
          source: {
            type: "string",
            enum: ["library"],
            description: "Adapter source.",
          },
          config: adapterConfigSchema(),
        },
        required: ["name", "source", "config"],
      },
    },
    handler: {
      execute(toolArgs, execution) {
        return execution.context.executeTool({
          functionName: "create_adapter",
          arguments: transformCreateAdapterArguments(toolArgs),
        });
      },
    },
  };
}

function enableAdapterTool(): ToolInstance {
  return hostTool({
    name: "enable_adapter",
    description: "Re-enable a disabled adapter for this conversation.",
    parameters: adapterIdParameters(
      "Adapter id returned by create_adapter or list_adapters.",
    ),
  });
}

function listAdaptersTool(): ToolInstance {
  return hostTool({
    name: "list_adapters",
    description:
      "List adapters for this conversation. Disabled adapters are hidden unless includeDisabled is true. Each record includes health fields: last_connected_at_ms and last_error. ExoChat records include chatUrl.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        includeDisabled: {
          type: ["boolean", "null"],
          description: "Whether to include disabled adapters.",
        },
      },
      required: ["includeDisabled"],
    },
  });
}

function disableAdapterTool(): ToolInstance {
  return hostTool({
    name: "disable_adapter",
    description:
      "Disable an adapter for this conversation while preserving its event history.",
    parameters: adapterIdParameters(
      "Adapter id returned by create_adapter or list_adapters.",
    ),
  });
}

function deleteAdapterTool(): ToolInstance {
  return hostTool({
    name: "delete_adapter",
    description:
      "Permanently delete an adapter for this conversation, including its stored event history.",
    parameters: adapterIdParameters(
      "Adapter id returned by create_adapter or list_adapters.",
    ),
  });
}

function sendAdapterMessageTool(): ToolInstance {
  return hostTool({
    name: "send_adapter_message",
    description:
      "Send an explicit outbound message through an adapter. For IRC this sends PRIVMSG to the adapter channel. For ExoChat, omit target or use the session channel id. For WhatsApp, provide target as the chat id from the inbound message. For Signal, provide a username such as u:example.01, a phone number, UUID, or group id. For Discord, provide a channel id unless defaultChannelId was configured. For Slack, provide a channel id, CHANNEL_ID:THREAD_TS, dm:USER_ID, or dm:USER_ID:THREAD_TS unless defaultChannelId was configured. Only call this when the user or conversation context makes the external side effect appropriate.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        adapterId: {
          type: "string",
          description:
            "Adapter id returned by create_adapter or list_adapters.",
        },
        text: {
          type: "string",
          description: "Message text to send through the adapter.",
        },
        target: {
          type: ["string", "null"],
          description:
            "External destination for adapters that need one. Use the target from the inbound wakeup when available; ExoChat accepts null or the session channel id, WhatsApp requires a chat id, Signal requires a username/phone/UUID/group id, Discord requires a channel id unless defaultChannelId was configured, and Slack accepts CHANNEL_ID, CHANNEL_ID:THREAD_TS, dm:USER_ID, or dm:USER_ID:THREAD_TS.",
        },
        attachments: {
          anyOf: [
            {
              type: "array",
              items: {
                type: "object",
                additionalProperties: false,
                properties: {
                  kind: {
                    type: "string",
                    enum: ["image", "video", "audio", "document"],
                    description:
                      "Attachment kind. Rich attachments are currently supported by the WhatsApp, Signal, and Discord adapters.",
                  },
                  url: {
                    type: ["string", "null"],
                    description:
                      "HTTPS URL for the media file. Specify exactly one of url, data, or sandboxPath.",
                  },
                  data: {
                    type: ["string", "null"],
                    description:
                      "Base64 media bytes, or a data: URL. Prefer sandboxPath for files created inside the sandbox.",
                  },
                  sandboxPath: {
                    type: ["string", "null"],
                    description:
                      "Path to a media file inside the active sandbox. Use this for files generated by shell commands in the sandbox.",
                  },
                  mimeType: {
                    type: ["string", "null"],
                    description:
                      "Optional MIME type. Required for WhatsApp documents and recommended for audio.",
                  },
                  fileName: {
                    type: ["string", "null"],
                    description:
                      "Optional file name. Required for WhatsApp documents.",
                  },
                },
                required: [
                  "kind",
                  "url",
                  "data",
                  "sandboxPath",
                  "mimeType",
                  "fileName",
                ],
              },
            },
            { type: "null" },
          ],
          description:
            "Optional rich media attachments. Use null for text-only messages.",
        },
      },
      required: ["adapterId", "text", "target", "attachments"],
    },
  });
}

function hostTool(args: {
  name: AdapterToolName;
  functionName?: string;
  description: string;
  parameters: ToolDefinition["parameters"];
}): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: args.name,
      description: args.description,
      parameters: args.parameters,
    },
    handler: {
      execute(toolArgs, execution) {
        return execution.context.executeTool({
          functionName: args.functionName ?? args.name,
          arguments: toolArgs,
        });
      },
    },
  };
}

function adapterConfigSchema(): ToolDefinition["parameters"] {
  return {
    anyOf: [
      {
        type: "object",
        additionalProperties: false,
        properties: {
          type: { type: "string", enum: ["irc"] },
          server: { type: "string" },
          port: { type: "number" },
          tls: { type: "boolean" },
          nick: { type: "string" },
          username: { type: "string" },
          realname: { type: "string" },
          channel: { type: "string" },
          passwordSecretId: { type: ["string", "null"] },
          trigger: {
            type: "string",
            enum: ["mention", "all_messages"],
            description:
              "Wake policy. Use mention unless the user explicitly wants every channel message.",
          },
        },
        required: [
          "type",
          "server",
          "port",
          "tls",
          "nick",
          "username",
          "realname",
          "channel",
          "passwordSecretId",
          "trigger",
        ],
      },
      {
        type: "object",
        additionalProperties: false,
        properties: {
          type: { type: "string", enum: ["slack"] },
          botTokenSecretId: {
            type: "string",
            description:
              "Secret name or id containing the Slack Bot User OAuth Token. The worker receives it as EXO_SLACK_BOT_TOKEN.",
          },
          signingSecretId: {
            type: "string",
            description:
              "Secret name or id containing the Slack Signing Secret. The worker receives it as EXO_SLACK_SIGNING_SECRET and verifies every Events API request.",
          },
          port: {
            type: "number",
            description:
              "Local HTTP port where the Slack worker listens. Use 3939 for the default ngrok setup.",
          },
          path: {
            type: "string",
            description:
              "Local HTTP request path for Slack Events API callbacks. Use /slack/events.",
          },
          defaultChannelId: {
            type: ["string", "null"],
            description:
              "Optional Slack channel id, CHANNEL_ID:THREAD_TS, dm:USER_ID, or dm:USER_ID:THREAD_TS used when send_adapter_message target is null.",
          },
          trigger: {
            type: "string",
            enum: ["all_messages", "mentions_only"],
            description:
              "Wake policy. Use mentions_only for local Slack testing; it wakes mentions, DMs if message.im is subscribed, and active thread follow-ups if channel message events are subscribed. all_messages wakes every subscribed channel message.",
          },
          allowedChannels: {
            anyOf: [
              { type: "array", items: { type: "string" } },
              { type: "null" },
            ],
            description:
              "Optional list of Slack channel ids to wake on. Use null to allow every channel Slack sends to this app.",
          },
          allowBots: {
            type: "boolean",
            description:
              "When true, messages from other Slack bot accounts wake this adapter. Defaults to false. The adapter never wakes on its own messages.",
          },
          threadReplies: {
            type: "boolean",
            description:
              "When true, inbound targets include the Slack thread timestamp as CHANNEL_ID:THREAD_TS so replies go to the same thread.",
          },
          progressMode: {
            type: "string",
            enum: ["update", "stream", "off"],
            description:
              "Slack progress UI mode. Use update to post a normal progress message and replace it with the final reply, stream for native threaded Slack streaming, or off to send only final messages.",
          },
          conversationScope: {
            type: "string",
            enum: ["adapter", "target"],
            description:
              "Conversation routing mode. Use target to create and wake a separate conversation per Slack target. Defaults to target.",
          },
        },
        required: [
          "type",
          "botTokenSecretId",
          "signingSecretId",
          "port",
          "path",
          "defaultChannelId",
          "trigger",
          "allowedChannels",
          "allowBots",
          "threadReplies",
          "progressMode",
          "conversationScope",
        ],
      },
      {
        type: "object",
        additionalProperties: false,
        properties: {
          type: { type: "string", enum: ["signal"] },
          account: {
            type: ["string", "null"],
            description:
              "Optional local signal-cli account identifier. Use null to have the worker start signal-cli link and discover the linked account.",
          },
          deviceName: {
            type: ["string", "null"],
            description:
              "Optional linked-device name when account is null. Use null for the default worker identity.",
          },
          configDir: {
            type: ["string", "null"],
            description:
              "Optional signal-cli config directory. Use null for the adapter state directory.",
          },
          trigger: {
            type: "string",
            enum: ["all_messages", "contacts_only"],
            description:
              "Wake policy. Use all_messages for the MVP unless allowedContacts is set.",
          },
          allowedContacts: {
            anyOf: [
              { type: "array", items: { type: "string" } },
              { type: "null" },
            ],
            description:
              "Optional list of Signal usernames, phone numbers, UUIDs, or group ids to wake on. Use null to allow all inbound messages.",
          },
        },
        required: [
          "type",
          "account",
          "deviceName",
          "configDir",
          "trigger",
          "allowedContacts",
        ],
      },
      {
        type: "object",
        additionalProperties: false,
        properties: {
          type: { type: "string", enum: ["whatsapp"] },
          authDir: {
            type: ["string", "null"],
            description:
              "Optional directory for Baileys auth state. Use null for the default under .exo.",
          },
          linkMethod: {
            type: ["string", "null"],
            enum: ["qr", "pairing-code", null],
            description:
              "Link method for first-time pairing. Use qr by default; use pairing-code when QR linking is unreliable.",
          },
          phoneNumber: {
            type: ["string", "null"],
            description:
              "Phone number for pairing-code linkMethod. Use null with qr.",
          },
          trigger: {
            type: "string",
            enum: ["all_messages", "contacts_only"],
            description:
              "Wake policy. Use all_messages for the MVP unless the user wants to ignore groups.",
          },
          allowedChats: {
            anyOf: [
              { type: "array", items: { type: "string" } },
              { type: "null" },
            ],
            description:
              "Optional list of WhatsApp chat ids to wake on. Use null to allow all chats permitted by trigger.",
          },
        },
        required: [
          "type",
          "authDir",
          "linkMethod",
          "phoneNumber",
          "trigger",
          "allowedChats",
        ],
      },
      {
        type: "object",
        additionalProperties: false,
        properties: {
          type: { type: "string", enum: ["discord"] },
          botTokenSecretId: {
            type: "string",
            description:
              "Secret name or id containing the Discord bot token. The worker receives it as EXO_DISCORD_BOT_TOKEN.",
          },
          defaultChannelId: {
            type: ["string", "null"],
            description:
              "Optional Discord channel id used when send_adapter_message target is null.",
          },
          trigger: {
            type: "string",
            enum: ["all_messages", "mentions_only"],
            description:
              "Wake policy. Use all_messages for test adapters unless the user explicitly wants mention-only wakeups.",
          },
          allowedChannels: {
            anyOf: [
              { type: "array", items: { type: "string" } },
              { type: "null" },
            ],
            description:
              "Optional list of Discord channel ids to wake on. Use null to allow every channel the bot can read.",
          },
          allowBots: {
            type: "boolean",
            description:
              "When true, messages from other bot accounts wake this adapter. Defaults to false (ignore all bots). The adapter never wakes on its own messages.",
          },
          voice: {
            type: "boolean",
            description:
              "Enable voice chat: the bot can join a voice channel and hold a spoken conversation (STT -> agent -> TTS). Defaults to false. When true, set openaiSecretId and ensure the bot has the applications.commands scope plus Connect/Speak permissions.",
          },
          openaiSecretId: {
            type: ["string", "null"],
            description:
              'Secret id holding the OpenAI API key used for voice STT/TTS. Use "openai" when voice is true; null when voice is false.',
          },
          conversationScope: {
            type: "string",
            enum: ["adapter", "target"],
            description:
              "Conversation routing mode. Use adapter to wake the adapter's root conversation for every message; use target to create and wake a separate conversation per Discord channel. Defaults to adapter.",
          },
        },
        required: [
          "type",
          "botTokenSecretId",
          "defaultChannelId",
          "trigger",
          "allowedChannels",
          "allowBots",
          "voice",
          "openaiSecretId",
          "conversationScope",
        ],
      },
      {
        type: "object",
        additionalProperties: false,
        properties: {
          type: { type: "string", enum: ["exochat"] },
          baseUrl: {
            type: ["string", "null"],
            description:
              "Base URL of the ExoChat WebSocket relay. Use null for the default hosted service.",
          },
          channelId: {
            type: ["string", "null"],
            description:
              "Optional stable relay channel id. Use null to generate and persist one in adapter state.",
          },
          secretId: {
            type: ["string", "null"],
            description:
              "Optional Exoharness secret reference containing the shared channel secret. Use null with channelId null to generate a host-stored secret.",
          },
        },
        required: ["type", "baseUrl", "channelId", "secretId"],
      },
      {
        type: "object",
        additionalProperties: false,
        properties: {
          type: { type: "string", enum: ["agent-cli"] },
          socketPath: {
            type: ["string", "null"],
            description:
              "Host unix socket path the worker listens on for exo-cli clients. Use null for the worker default.",
          },
          mountRoot: {
            type: "string",
            description:
              "Absolute host directory that is bind-mounted read-write into the agent sandbox (e.g. /Users/me/projects). exo-cli invocations from directories outside this root cannot access files.",
          },
          mountPath: {
            type: ["string", "null"],
            description:
              "Sandbox path where mountRoot is mounted. Use null for the default /agent-cli. Must match the sandbox mount configured on the conversation.",
          },
        },
        required: ["type", "socketPath", "mountRoot", "mountPath"],
      },
      {
        type: "object",
        additionalProperties: false,
        properties: {
          type: { type: "string", enum: ["trial"] },
          socketPath: {
            type: "string",
            description:
              "Absolute host unix socket path on which an evaluation runner submits containerized trials.",
          },
        },
        required: ["type", "socketPath"],
      },
    ],
  } as ToolDefinition["parameters"];
}

function transformCreateAdapterArguments(args: JsonObject): JsonObject {
  const config = objectField(args, "config");
  validateAdapterSource(
    stringField(args, "source"),
    stringField(config, "type"),
  );
  return args;
}

function validateAdapterSource(source: string, type: string): void {
  if (
    (type === "irc" ||
      type === "agent-cli" ||
      type === "exochat" ||
      type === "whatsapp" ||
      type === "signal" ||
      type === "discord" ||
      type === "slack" ||
      type === "trial") &&
    source !== "library"
  ) {
    throw new Error(`${type} adapters must use source 'library'`);
  }
}

function adapterIdParameters(
  description: string,
): ToolDefinition["parameters"] {
  return {
    type: "object",
    additionalProperties: false,
    properties: {
      adapterId: {
        type: "string",
        description,
      },
    },
    required: ["adapterId"],
  };
}

function stringField(args: JsonObject, name: string): string {
  const value = args[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`adapter argument ${name} must be a non-empty string`);
  }
  return value;
}

function objectField(args: JsonObject, name: string): JsonObject {
  const value = args[name];
  if (!isRecord(value)) {
    throw new Error(`adapter argument ${name} must be an object`);
  }
  return value;
}

function isRecord(value: unknown): value is JsonObject {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
