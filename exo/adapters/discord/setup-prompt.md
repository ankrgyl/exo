Set up a Discord adapter for testing.

Create a library Discord adapter if one does not already exist for this conversation, then make sure it is ready for the background adapter runner. Use these settings:

- name: `discord-dev`
- source: `library`
- type: `discord`
- botTokenSecretId: `discord-bot-token`
- defaultChannelId: `null`
- trigger: `all_messages`
- allowedChannels: `null`
- allowBots: `false`
- voice: `false` (set to `true` only if I ask for voice chat; when enabled also set `openaiSecretId: "openai"`, tell me to invite the bot with the `applications.commands` scope plus Connect/Speak permissions, and to store the OpenAI key with `exo secret set openai --env OPENAI_API_KEY`)
- openaiSecretId: `null` (use `"openai"` only when voice is `true`)
- conversationScope: `adapter`

Do not block on secret inspection. The harness may not expose a secret-listing tool, so assume secret `discord-bot-token` exists and create the adapter. If the adapter later reports a missing Discord token, tell the user to create the secret with `exo secret set discord-bot-token --env DISCORD_BOT_TOKEN` after exporting a Discord bot token locally.

After creating or confirming the adapter, briefly explain that the Discord bot must be invited to the target server with message read/send permissions and the Message Content Intent enabled in the Discord Developer Portal. Tell me the adapter id and what Discord channel id to use as `target` for a test `send_adapter_message` call.
