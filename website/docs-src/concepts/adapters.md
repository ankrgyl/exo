---
title: Adapters
description: Supervised connections that receive external events and wake conversations.
---

# Adapters

Adapters are long-running, host-managed bridges to external systems that let an
agent receive messages from outside the REPL and send explicit replies back:

- `exochat` — a hosted, text-only browser chat at `https://exoharness.ai`;
  the canonical setup starts it by default and prints its URL
- `irc` — IRC channels
- `whatsapp` — WhatsApp linked device (Baileys)
- `signal` — Signal linked device (`signal-cli`)
- `discord` — Discord bot with message, attachment, and optional voice support
- `slack` — Slack bot via Events API
- `agent-cli` — a local shell adapter for sending prompts from any directory

They are deliberately separate from [tools](./tools):

- A **tool** runs for a bounded model call during a turn.
- An **adapter** remains available between turns.
- An inbound adapter event can wake a conversation.
- Outbound effects require an explicit tool call; final model text is never
  sent implicitly.

## Workers and durable state

Adapters run as supervised worker processes. The host owns lifecycle, routing,
durable inbox/outbox records, retries, and conversation wakeups. Each worker
owns its protocol-specific connection, event normalization, and
acknowledgements.

Lifecycle states are independent of model turns: starting, running, disabled,
or error. Adapter identity, provider cursors, deduplication keys, routing,
pending sends, and health survive worker restarts.

Outbound messages are durable records with explicit delivery states:

1. **queued** — accepted by the host
2. **in-flight** — claimed by the worker
3. **delivered** or **failed** — terminal

A rejected delivery is retried up to three attempts before it becomes a
terminal failure. Delivered and failed records are retained for inspection.
Queuing an outbound message is not the same as provider-confirmed delivery.

Credentials are secret references in adapter configuration (for example
`botTokenSecretId` or ExoChat `secretId`). The host resolves their values when
starting an authorized worker; raw credentials do not belong in prompts or
durable configuration.

Protocol-specific code lives with each adapter under
[`examples/exo/adapters/`](https://github.com/exoharness/exo/tree/main/examples/exo/adapters).
Adapter records, event history, and the outbound queue are stored under
`.exo/adapters/`.

## Managing adapters

The agent manages its own adapters through these tools:

- `create_adapter` — create and enable a shipped adapter from a name, source,
  and per-type config
- `list_adapters` — list adapters, including health fields
  (`last_connected_at_ms`, `last_error`); ExoChat records include `chatUrl`
- `enable_adapter` — re-enable a disabled adapter without recreating it
- `disable_adapter` — stop an adapter but keep its event history
- `delete_adapter` — remove an adapter and its history entirely
- `send_adapter_message` — send an explicit outbound message (text plus
  optional image/video/audio/document attachments) through an adapter

Related inspection tools:

- `list_adapter_events` — durable adapter events (connected, disconnected,
  inbound, outbound, error), filterable by type and time
- `list_conversation_events` — what actually woke the conversation, which
  separates "the adapter received it" from "the conversation was woken"

So configuring an adapter is usually a conversation: store any credentials,
then ask the agent to create the adapter, and it calls `create_adapter`
itself.

`create_adapter` currently accepts only `source: "library"`. There are no
built-in adapter implementations in the host; every shipped type is a library
worker module under `examples/exo/adapters/<type>/worker.ts`.

A universal adapter command envelope and manifest-generated model tools are
deferred. Protocol-specific commands may remain explicit tools until there is
enough experience to justify a generic command system. Remote adapter
registries and signed distribution are also outside the current plan.

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
   ./exo.sh --setup discord
   ```

   For adapters that link as a device (WhatsApp, Signal), the setup path
   watches the adapter log and prints the QR code to scan from your phone.

3. **Verify.** `list_adapters` shows each adapter's `last_connected_at_ms`
   and `last_error`, then send a test message through
   `send_adapter_message`.

### The adapter types

| Type | Credentials / linking | Wake trigger options |
|:-----|:----------------------|:---------------------|
| `exochat` | Optional `secretId`; null generates and stores a host secret | Every chat message |
| `irc` | Optional server password via `passwordSecretId` | `mention`, `all_messages` |
| `whatsapp` | Linked device — QR scan or pairing code | `all_messages`, `contacts_only`; optional `allowedChats` |
| `signal` | Linked device — `signal-cli` QR scan | `all_messages`, `contacts_only`; optional `allowedContacts` |
| `discord` | Bot token via `botTokenSecretId` | `all_messages`, `mentions_only`; optional `allowedChannels`, `allowBots` |
| `slack` | Bot token via `botTokenSecretId` plus signing secret via `signingSecretId` | `all_messages`, `mentions_only`; optional `allowedChannels`, `allowBots` |
| `agent-cli` | None — local unix socket plus a host directory bind-mounted into the sandbox | Every `exo-cli` invocation |

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
    "voice": false,
    "openaiSecretId": null,
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
channel for IRC, link method for WhatsApp, allowed contacts for Signal,
Events API port/path for Slack, and the mount root for agent-cli.

::: info
  Each adapter has a full setup walkthrough (bot creation, permissions,
  linking, troubleshooting) in its README under
  [`examples/exo/adapters/`](https://github.com/exoharness/exo/tree/main/examples/exo/adapters)
  — start there when setting one up for real.
:::

## Targeting outbound messages

Outbound sends need to say *where* the message goes. Each adapter has its
own target format:

- WhatsApp — chat id
- Signal — username, phone number, UUID, or group id
- Discord — channel id
- Slack — channel id, `CHANNEL_ID:THREAD_TS`, `dm:USER_ID`, or
  `dm:USER_ID:THREAD_TS`
- ExoChat — omit target or use the session channel id
- IRC — the configured channel

The inbound wakeup carries the right value, so the normal pattern is to reply
using the `target` from the message that woke the conversation.

If an outbound message stays queued, the worker is not claiming work. Repeated
provider failures become a terminal failed record after three attempts, with
the error retained. Use `disable_adapter` / `enable_adapter` to restart a
misbehaving worker without losing history; use `delete_adapter` only when the
history should go too.

## Scheduler

The scheduler remains the current scheduler service with its existing tools.
It stores schedules and wakes conversations when work is due, but it is not
modeled as an adapter.

Moving scheduling onto the adapter runtime can be reconsidered later.
Practical profiles include the current scheduler service directly rather than
depending on that future design.

## Profiles

- **Bootstrap** installs no adapters.
- **Practical** starts the current scheduler service and the installation's
  selected adapters.

Profiles are curated defaults. They do not imply remote discovery, signatures,
or a capability sandbox for worker code.
