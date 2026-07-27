# Discord Adapter

The Discord adapter is an experimental Exo library adapter implemented as a TypeScript worker using `discord.js`. It logs in as a Discord bot, emits inbound Discord messages as adapter wakeups, and sends explicit outbound messages through `send_adapter_message`.

## Setup

### 1. Create a Discord Bot

1. Open the Discord Developer Portal: <https://discord.com/developers/applications>.
2. Click **New Application**, give it a name, and open the new application.
3. Open **Bot** in the left sidebar.
4. Click **Reset Token** or **View Token**, then copy the bot token. Keep this token private.
5. In **Privileged Gateway Intents**, enable **Message Content Intent**. This is required for Exo to read message text.

### 2. Invite the Bot to a Server

1. Open **OAuth2** > **URL Generator** in the Developer Portal.
2. Under **Scopes**, select `bot`.
3. Under **Bot Permissions**, select at least:
   - `View Channels`
   - `Send Messages`
   - `Read Message History`
   - `Attach Files` if you want Exo to send attachments
4. Copy the generated URL, open it in a browser, and add the bot to your Discord server.
5. In Discord, make sure the bot can see the target channel. Copy the channel id for testing:
   - Enable Discord developer mode in **User Settings** > **Advanced** > **Developer Mode**.
   - Right-click the target channel and choose **Copy Channel ID**.

### 3. Store the Bot Token in Exo

Export the token locally and store it as an Exo secret:

```bash
export DISCORD_BOT_TOKEN="..."
exo secret set discord-bot-token --env DISCORD_BOT_TOKEN
```

The setup prompt below expects the secret name to be `discord-bot-token`.

### 4. Create the Exo Adapter

Run the Exo setup prompt:

```bash
./exo.sh --setup discord
```

If you are setting up a fresh local Exo agent, use the same flags you normally use for your agent/conversation, for example:

```bash
./exo.sh \
  --agent exospooky \
  --conversation dev \
  --setup discord
```

The setup prompt at `setup-prompt.md` asks Exo to create a library adapter similar to:

```json
{
  "name": "discord-dev",
  "source": "library",
  "config": {
    "type": "discord",
    "botTokenSecretId": "discord-bot-token",
    "defaultChannelId": null,
    "trigger": "all_messages",
    "allowedChannels": null,
    "allowBots": false,
    "conversationScope": "adapter"
  }
}
```

Use `defaultChannelId` when you want outbound messages to go to one channel by default. Otherwise, pass the copied Discord channel id as `target` when calling `send_adapter_message`.

Set `allowBots: true` if you want the adapter to wake on messages from other bot accounts (useful for bot-to-bot integrations). The adapter never wakes on its own messages regardless of this flag.

Set `conversationScope: "target"` to wake a separate Exo conversation per Discord channel id. The default, `"adapter"`, preserves the historical behavior where every channel for an adapter shares the adapter's root conversation.

### 5. Test It

Ask Exo to send a Discord message with the adapter id returned by setup:

```text
Send "hello from exo" to Discord using adapter <adapter-id> and target <channel-id>.
```

To test inbound wakeups with the default `all_messages` trigger, send any normal message in a channel the bot can read. If you configure `mentions_only`, mention the bot instead:

```text
@YourBot hello exo
```

## Configuration

- `botTokenSecretId` is the Exo secret name or id containing the Discord bot token.
- `defaultChannelId` is used when `send_adapter_message` is called with `target: null`.
- `trigger` is either `mentions_only` or `all_messages`. Direct messages always trigger.
- `allowedChannels` optionally restricts inbound wakeups to specific Discord channel ids.
- `voice` enables voice chat (default `false`). See below.
- `openaiSecretId` is the secret holding the OpenAI API key used for voice STT/TTS. Defaults to `openai` when `voice` is enabled.

Outbound messages support text plus the shared adapter attachment forms: `sandboxPath`, HTTPS `url`, or base64/data URL `data`.

## Voice

With `voice: true` the bot can join a Discord voice channel and hold a spoken
conversation: it transcribes what you say, runs it through the normal Exo
agent turn (tools, identity, history), and speaks the reply back. STT and TTS
both use the existing OpenAI secret — no extra API key. See
[`voice-design.md`](./voice-design.md) for the architecture.

Extra requirements vs. the text-only adapter:

1. Invite the bot with the `applications.commands` scope in addition to `bot`,
   and grant the **Connect** and **Speak** permissions.
2. The adapter adds the `Guild Voice States` gateway intent automatically when
   `voice` is enabled (not a privileged intent).
3. Bind the OpenAI key as a secret (defaults to the id `openai`):

   ```bash
   exo secret set openai --env OPENAI_API_KEY
   ```

Create the adapter with voice on:

```json
{
  "name": "discord-dev",
  "source": "library",
  "config": {
    "type": "discord",
    "botTokenSecretId": "discord-bot-token",
    "defaultChannelId": null,
    "trigger": "all_messages",
    "allowedChannels": null,
    "allowBots": false,
    "voice": true,
    "openaiSecretId": "openai"
  }
}
```

Then, in Discord:

- Join a voice channel and run `/voice join`. The bot joins your channel.
- Talk. Each utterance (ended by a short silence) is transcribed, handled by the
  agent, and the reply is spoken back. The transcript and reply are also posted
  as text in the channel.
- Run `/voice leave` to disconnect. The bot also leaves automatically when the
  channel empties.

Voice is assistant-grade latency (a few seconds per turn, more when the agent
runs tools), not a low-latency phone call. Replies are kept short and plain so
they read well as speech.

## Rich Attachments

Discord supports outbound image, video, audio, and document attachments through `send_adapter_message`. Prefer `sandboxPath` for files created by shell commands in the Exo sandbox:

```json
{
  "adapterId": "<discord-adapter-id>",
  "target": "<discord-channel-id>",
  "text": "Here is the generated file.",
  "attachments": [
    {
      "kind": "document",
      "url": null,
      "data": null,
      "sandboxPath": "/tmp/report.txt",
      "mimeType": "text/plain",
      "fileName": "report.txt"
    }
  ]
}
```

For files created in the sandbox, use `sandboxPath`. For remote media, use an HTTPS `url`. For small inline payloads, use base64 `data` or a data URL.

### Inbound Attachments

Inbound Discord attachments are forwarded with the message wakeup:

- Images (up to 4 per message, 8 MB each) are downloaded by the host and attached to the wakeup message as multimodal content, so the model can view and analyze them directly.
- Every attachment (including videos, audio, and documents) is also listed in the wakeup prompt with its kind, file name, mime type, and Discord CDN URL. Discord CDN URLs carry expiring signatures, so download promptly (e.g. with `curl` in the sandbox) if the raw file is needed.
- Oversized or undownloadable images fall back to the URL listing instead of failing the message.
