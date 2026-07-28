---
title: Adapters
description: Long-running connections to external channels — what they are and how to configure each one.
---

# Adapters

Adapters are long-running, host-managed connections to external surfaces
that let an agent receive messages from outside the REPL and send explicit
replies back:

- `exochat` — a hosted, text-only browser chat at `https://exoharness.ai`;
  the canonical setup starts it by default and prints its URL
- `irc` — IRC channels
- `whatsapp` — WhatsApp linked device (Baileys)
- `signal` — Signal linked device (`signal-cli`)
- `discord` — Discord bot with message, attachment, and optional voice support
- `agent-cli` — a local shell adapter for sending prompts from any directory

They are deliberately separate from tools:

- **Tools** run during a model turn.
- **Adapters** run continuously in a background host process.
- Inbound adapter events wake a conversation by creating a normal turn.
- Outbound sends are **explicit tool calls**, never implicit model output.

For how the adapter runner supervises workers, drains/restarts, and how
create → enable → disable → delete interacts with conversations, see
[Lifecycles → Adapters](./lifecycles#adapter-lifecycle).

## Workers

Adapters run as supervised worker processes speaking JSONL over
stdin/stdout. Protocol-specific code lives with the adapter (e.g.
`examples/exo/adapters/<adapter>/`), not in the shared Rust runtime.
Adapter records, event history, and the outbound queue are stored under
`.exo/adapters/`.

## Managing adapters

The agent manages its own adapters through five tools:

- `create_adapter` — create and enable an adapter from a name, source, and
  per-type config (below)
- `list_adapters` — list adapters, including health fields
  (`last_connected_at_ms`, `last_error`)
- `disable_adapter` — stop an adapter but keep its event history
- `delete_adapter` — remove an adapter and its history entirely
- `send_adapter_message` — send an explicit outbound message (text plus
  optional image/video/audio/document attachments) through an adapter

So "configuring an adapter" is usually a conversation: you store any
credentials, then ask the agent to create the adapter, and it calls
`create_adapter` itself.

## Configuring an adapter

The general recipe:

1. **Store credentials as a secret.** Adapter configs never contain raw
   tokens — they reference [secrets](./bindings-and-secrets) by name:

   ```bash
   export DISCORD_BOT_TOKEN="..."
   exo secret set discord-bot-token --env DISCORD_BOT_TOKEN
   ```

2. **Create the adapter.** Ask the agent to create it, or use the shipped
   per-adapter setup prompts:

   ```bash
   examples/exo/scripts/exo-control --setup discord
   ```

   For adapters that link as a device (WhatsApp, Signal), the setup script
   watches the adapter log and prints the QR code to scan from your phone.

3. **Verify.** `list_adapters` shows each adapter's `last_connected_at_ms`
   and `last_error`, then send a test message through
   `send_adapter_message`.

### The adapter types

| Type | Source | Credentials / linking | Wake trigger options |
|:-----|:-------|:----------------------|:---------------------|
| `exochat` | — | None — started by the canonical setup | Every chat message |
| `irc` | `built_in` | Optional server password via `passwordSecretId` | `mention`, `all_messages` |
| `whatsapp` | `library` | Linked device — QR scan or pairing code | `all_messages`, `contacts_only`; optional `allowedChats` |
| `signal` | `library` | Linked device — `signal-cli` QR scan | `all_messages`, `contacts_only`; optional `allowedContacts` |
| `discord` | `library` | Bot token via `botTokenSecretId` | `all_messages`, `mentions_only`; optional `allowedChannels`, `allowBots` |
| `agent-cli` | `built_in` | None — local unix socket + a host directory bind-mounted into the sandbox | Every `exo-cli` invocation |

`source` tells `create_adapter` where the worker code comes from: `built_in`
adapters ship with the harness runtime; `library` adapters are shipped
worker modules (`examples/exo/adapters/<type>/worker.ts`).

### A worked example: Discord

With the bot token stored as the secret `discord-bot-token` (step 1 above),
the agent creates the adapter with a config like:

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

The knobs that matter:

- **`trigger`** — when inbound messages wake the conversation
  (`mentions_only` vs `all_messages`; direct messages always wake).
- **`defaultChannelId`** — where `send_adapter_message` sends when no
  `target` is given; otherwise the agent passes the channel id from the
  inbound wakeup as `target`.
- **`conversationScope`** — `adapter` wakes one root conversation for every
  channel; `target` creates a separate conversation per Discord channel.
- **`voice`** — lets the bot join a voice channel and hold a spoken
  conversation (STT → agent turn → TTS); requires an OpenAI key secret via
  `openaiSecretId`.

The other types follow the same shape with their own fields — server and
channel for IRC, link method for WhatsApp, allowed contacts for Signal, the
mount root for agent-cli.

::: info
  Each adapter has a full setup walkthrough (bot creation, permissions,
  linking, troubleshooting) in its README under
  [`examples/exo/adapters/`](https://github.com/exoharness/exo/tree/main/examples/exo/adapters)
  — start there when setting one up for real.
:::

## Targeting outbound messages

Outbound sends need to say *where* the message goes. Each adapter has its
own target format: a WhatsApp chat id, a Signal username / phone number /
group id, a Discord channel id. The inbound wakeup carries the right value,
so the normal pattern is to reply using the `target` from the message that
woke the conversation.
