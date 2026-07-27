# Exo Email Adapter Architecture

This document proposes email as an Exo adapter backed by Resend. Email is
bidirectional and event-driven: inbound messages arrive while the agent is idle,
and outbound messages should be queued and delivered through a durable external
integration. That matches the existing adapter model better than a pure
synchronous tool.

The main design is:

```text
Resend inbound webhook
  -> email adapter worker
  -> AdapterStore inbound event
  -> Exo conversation wakeup
  -> agent decides what to do
  -> send_adapter_message
  -> AdapterStore outbound queue
  -> email adapter worker sends via Resend
```

Optional library tools can still exist later for inbox inspection, but the
primary integration should be `exo/adapters/email/`.

## Goals

- Add first-party email support using the existing Exo adapter subsystem.
- Support inbound email, outbound email, replies, and attachments.
- Use Resend for outbound delivery and inbound routing/webhooks.
- Keep email-specific code under `exo/`.
- Keep secrets and deploy-specific defaults out of Git.
- Reuse `create_adapter`, `list_adapters`, `disable_adapter`, `delete_adapter`,
  and `send_adapter_message` where possible.
- Keep wakeups compact and make the agent explicitly decide whether to reply.

## Non-Goals

- Do not add email as a core Exoharness concept.
- Do not add a generic CLI email command.
- Do not make email a TypeScript library tool as the primary abstraction.
- Do not implement a full inbox, contact manager, or CRM in the first version.
- Do not require the `resend` npm package unless it materially simplifies the
  implementation.

## Proposed Location

```text
exo/adapters/email/
  README.md
  setup-prompt.md
  worker.ts
  resend.ts
  webhook-server.ts
  email-store.ts
```

Shared adapter protocol changes, if any, should stay in:

```text
exo/adapters/protocol.ts
crates/executor/src/adapter/
exoharness/typescript/harness/adapter-tools.ts
```

The first pass should try to fit inside the current adapter protocol:

- inbound email maps to `WorkerEvent::Message`.
- outbound email maps to `WorkerCommand::SendMessage`.
- email attachments reuse `AdapterAttachment`.

Only extend the Rust/TypeScript adapter protocol if email needs fields that
cannot safely fit into the existing `metadata` JSON.

## Why Adapter, Not Tool

A tool is invoked during an active agent turn. It is good for actions the agent
initiates, such as "send this email now."

Receiving email is different:

- It happens while the agent is idle.
- It needs a long-running webhook listener or externally reachable endpoint.
- It must deduplicate provider retries.
- It should wake the conversation when a message arrives.
- It needs durable integration state independent of the current turn.

Those are adapter responsibilities. Email should therefore be an adapter first,
with any tools layered on later as convenience helpers.

## Adapter Configuration

Add an email adapter config shape that the existing `create_adapter` tool can
store in `AdapterConfig.settings`.

Suggested settings:

```json
{
  "adapterType": "email",
  "settings": {
    "provider": "resend",
    "from": "Exo <agent@example.com>",
    "replyTo": "agent@example.com",
    "inbound": {
      "bind": "127.0.0.1:8765",
      "publicWebhookUrl": "https://example.ngrok.app/email",
      "webhookPath": "/email",
      "secretEnv": "RESEND_WEBHOOK_SECRET"
    },
    "routing": {
      "mode": "singleConversation",
      "addresses": ["agent@example.com"]
    },
    "allow": {
      "recipientDomains": ["example.com"],
      "recipients": [],
      "senders": []
    },
    "maxAttachmentBytes": 10485760
  },
  "secrets": {
    "RESEND_API_KEY": "<secret-id>",
    "RESEND_WEBHOOK_SECRET": "<secret-id>"
  }
}
```

Initial implementation can be simpler:

- one adapter instance
- one configured conversation
- one or more inbound addresses routed to that conversation
- one Resend API key
- one webhook secret

Address-based routing can come later if needed.

## Secrets And Local Config

Required secrets:

- `RESEND_API_KEY`: API key for sending email and calling Resend APIs.
- `RESEND_WEBHOOK_SECRET`: shared secret for webhook verification.

Required non-secret config:

- `from`: default sender address.
- `inbound.bind`: local bind address for the webhook server.
- `inbound.webhookPath`: HTTP path to receive webhooks.

Optional config:

- `replyTo`: default reply-to address.
- `publicWebhookUrl`: public URL to register in Resend.
- `allow.recipientDomains`: outbound recipient domain allowlist.
- `allow.recipients`: exact outbound recipient allowlist.
- `allow.senders`: inbound sender allowlist.
- `maxAttachmentBytes`: max total outbound or inbound attachment bytes.

Secrets should be stored through the existing adapter secret mechanism. Local
setup prompts and `.env` can help users configure the initial adapter, but
committed files should not contain real addresses, API keys, or personalized
settings.

## Inbound Flow

The email worker starts a small HTTP server and receives Resend webhooks.

Flow:

1. Resend receives an email for a configured domain/address.
2. Resend posts an inbound event to the adapter webhook endpoint.
3. The worker verifies the webhook secret/signature.
4. The worker normalizes the provider payload.
5. The worker deduplicates by provider message id.
6. The worker stores the full email payload and any staged attachments.
7. The worker emits a `message` worker event to the adapter runtime.
8. The adapter runtime records the inbound event and wakes the conversation.

The wakeup prompt should be compact:

```text
Email received.

Adapter id: <adapter-id>
Email id: <local-email-id>
From: <sender>
To: <recipient list>
Subject: <subject>
Preview:
<short text preview>

Attachments: <count>

Use send_adapter_message with this adapter id and target <reply-target> only if
you intentionally want to reply. Read stored email details before acting on long
or sensitive content.
```

The existing `WorkerEvent::Message` fields can map as:

- `target`: reply target, probably the local email id or sender address with
  metadata for threading.
- `sender`: normalized `from`.
- `text`: compact preview and key email metadata.
- `message_id`: provider message id or local email id.
- `metadata`: full structured metadata, including local email id, headers,
  subject, recipients, and attachment references.

## Outbound Flow

The agent sends email through the existing `send_adapter_message` tool.

For a new email:

```json
{
  "adapterId": "<email-adapter-id>",
  "target": "person@example.com",
  "text": "Hello from Exo.",
  "attachments": []
}
```

For a reply, the target should identify the inbound email or thread:

```json
{
  "adapterId": "<email-adapter-id>",
  "target": "email:<local-email-id>",
  "text": "Thanks, I will take a look.",
  "attachments": []
}
```

The email worker drains outbound messages from `AdapterStore` and maps them to
Resend's send API. It should support:

- new outbound email by recipient address
- reply by local email id
- reply by explicit metadata, if needed later
- text body
- optional HTML body via metadata, if the protocol is extended
- attachments using existing `AdapterAttachment`

If the existing outbound command only has `target`, `text`, and `attachments`,
the first version can treat `text` as the plain text body and use metadata-free
defaults for subject:

- new outbound subject: require the agent to include `subject` in metadata once
  protocol support exists, or use a conservative default and document the
  limitation.
- reply subject: derive from stored inbound email (`Re: <subject>`).

This suggests one small adapter protocol extension may be worthwhile: allow
`send_adapter_message` to accept optional `metadata` for adapter-specific fields
such as email subject, cc, bcc, reply-to, and html. If we add that, keep it a
generic JSON field rather than email-specific Rust types.

## Suggested `send_adapter_message` Metadata

If we extend outbound messages, add optional metadata:

```json
{
  "subject": "Status update",
  "cc": ["cc@example.com"],
  "bcc": [],
  "replyTo": "agent@example.com",
  "html": "<p>Hello</p>",
  "inReplyToEmailId": "<local-email-id>"
}
```

Rust changes:

- add `metadata: serde_json::Value` or `Option<Value>` to
  `AdapterOutboundMessageRecord`.
- add `metadata` to `WorkerCommand::SendMessage`.
- expose `metadata` in `exoharness/typescript/harness/adapter-tools.ts`.
- keep validation adapter-specific in the worker where possible.

This keeps the core adapter protocol generic and lets future adapters use
structured outbound metadata too.

## Email Store

Use an email-specific store under the adapter root rather than the conversation
event log.

Suggested layout:

```text
.exo/adapters/email/<adapter-id>/
  received/
    <email-id>.json
  sent/
    <email-id>.json
  blobs/
    <email-id>/
      body.txt
      body.html
      attachments/
```

Received record fields:

- `id`
- `adapterId`
- `provider`
- `providerMessageId`
- `from`
- `to`
- `cc`
- `subject`
- `textPreview`
- `textPath`
- `htmlPath`
- `headers`
- `attachments`
- `receivedAt`
- `readAt`
- `wakeupEventId` or wakeup metadata if useful

Sent record fields:

- `id`
- `adapterId`
- `provider`
- `providerMessageId`
- `target`
- `to`
- `cc`
- `bcc`
- `subject`
- `replyToEmailId`
- `sentAt`
- `attachments`

This mirrors existing adapter and scheduler design: subsystem state lives in a
subsystem store, while the conversation receives compact prompts and can inspect
details through tools or metadata.

## Attachments

Outbound attachments should reuse `AdapterAttachment`:

- `sandboxPath`: preferred for files generated by the agent.
- `path`: host-visible files staged by the runtime.
- `url`: remote files staged by the runtime if supported.
- `data`: small inline payloads only.

The worker should convert attachments into Resend's expected attachment shape,
usually filename plus base64 content.

Inbound attachments should be staged by the email worker into the email store.
The wakeup prompt should include attachment count and names, not full content.
Later, optional helper tools can expose attachments as artifacts or staged paths.

## Optional Helper Tools

Even with an adapter-first design, a small library tool module may still be
useful for inspecting stored email:

```text
exo/tools/library/email/
  index.ts
  store.ts
```

Potential helper tools:

- `list_received_emails`
- `read_received_email`
- `list_sent_emails`

These tools would read the email adapter store and return compact results or
artifact references. They should not own receiving or sending. Sending should go
through `send_adapter_message` so email behaves like IRC, WhatsApp, and Signal.

This can be phase two. The first adapter version can include enough detail in
the wakeup prompt and metadata to be useful without extra tools.

## Resend Integration

Outbound endpoint:

```text
POST https://api.resend.com/emails
```

Headers:

```text
Authorization: Bearer $RESEND_API_KEY
Content-Type: application/json
```

Outbound payload fields to map:

- `from`
- `to`
- `cc`
- `bcc`
- `reply_to`
- `subject`
- `text`
- `html`
- `attachments`
- provider-specific reply/threading headers if supported

Inbound should use Resend inbound routing/webhook support. The worker should
isolate provider-specific parsing in `resend.ts` so a future provider can be
added without changing the adapter protocol.

Normalized inbound fields:

- provider
- provider message id
- from
- to
- cc
- subject
- text body
- HTML body
- headers needed for replies/threading
- attachments metadata and staged content
- received timestamp

## Safety Model

Email is external and often sensitive. The adapter should be conservative:

- Verify inbound webhooks.
- Deduplicate webhook retries before waking the agent.
- Keep wakeup prompts compact.
- Do not dump long email bodies or attachments directly into prompts.
- Do not include API keys or full provider payloads in logs.
- Support outbound recipient allowlists.
- Support inbound sender allowlists.
- Treat inbound email as notification, not permission to reply.
- Require explicit `send_adapter_message` calls for all replies.
- Record sent email metadata for audit.

The Exo prompt should tell the agent:

- email messages can wake the conversation
- it should not auto-reply unless the user or standing instructions make that
  appropriate
- it should use `send_adapter_message` for intentional email replies
- it should preserve recipient privacy and avoid including sensitive content in
  unrelated channels

## Setup Flow

The setup prompt should mirror other adapters:

```text
Create an email adapter using Resend.

Ask the user for:
- Resend API key secret
- from address
- inbound webhook secret
- local bind address or public webhook URL
- inbound address/domain
- optional allowlists
```

Manual setup should be possible through the existing adapter tools once the
adapter type exists:

```text
create_adapter({
  "adapterType": "email",
  "name": "email",
  "trigger": { "type": "all" },
  "settings": { ... },
  "secrets": { ... }
})
```

The Exo startup script can later add `email` to `--adapters` alongside
`irc`, `whatsapp`, and `signal`.

## File-Level Plan

Phase 1: adapter skeleton.

- Add `exo/adapters/email/README.md`.
- Add `exo/adapters/email/setup-prompt.md`.
- Add `exo/adapters/email/worker.ts`.
- Add `exo/adapters/email/resend.ts`.
- Add `exo/adapters/email/email-store.ts`.
- Update docs to mention `email` as a supported adapter.

Phase 2: inbound receive.

- Implement local webhook server in the worker.
- Verify webhook secret/signature.
- Normalize Resend inbound events.
- Store received email records and blobs.
- Deduplicate provider message ids.
- Emit adapter `message` events with compact previews and metadata.

Phase 3: outbound send.

- Implement Resend send API client.
- Drain outbound messages from the adapter runtime.
- Support plain text new emails.
- Support replies using stored inbound email ids.
- Record sent email metadata.

Phase 4: metadata and richer outbound email.

- Decide whether to extend `send_adapter_message` with generic outbound
  `metadata`.
- Support subject, cc, bcc, reply-to, html, and reply/threading fields.
- Keep the Rust protocol generic and validate email-specific metadata in the
  email worker.

Phase 5: attachments.

- Map outbound `AdapterAttachment` values to Resend attachments.
- Stage inbound attachments in the email store.
- Add size limits and clear errors.

Phase 6: optional helper tools.

- Add `exo/tools/library/email/index.ts` only if needed.
- Implement `list_received_emails` and `read_received_email`.
- Keep sending through `send_adapter_message`.

Phase 7: verification and docs.

- Unit test Resend payload mapping.
- Unit test inbound webhook normalization.
- Unit test email store dedupe.
- Add worker-level smoke tests where practical.
- Update `exo/docs/adapter-architecture.md`.
- Update `exo/README.md`.
- Update `exo/prompts/me.md`.

## Open Questions

- Should v1 route all inbound email to one configured conversation, or support
  address-based routing immediately?
- Does Resend provide enough inbound webhook signature data for strong
  verification, or should we require an unguessable webhook path plus shared
  secret?
- Should outbound email require generic `metadata` support before v1, or can v1
  start with replies and plain text bodies?
- Should received emails be exposed only through wakeup metadata, or should v1
  include helper tools to read stored messages?
- Should the email adapter use the existing `AdapterStore` only, or add a
  sibling email-specific store under the adapter root?

## Recommendation

Implement email as `exo/adapters/email/`. Treat receiving as the
primary reason for choosing the adapter abstraction: a Resend webhook worker
stores inbound email, deduplicates events, and wakes the configured Exo
conversation. Treat sending as the adapter's outbound path through
`send_adapter_message`, not as a standalone `send_email` tool.

Add optional helper tools only after the adapter works, and only for richer inbox
inspection. Keep email out of core Exoharness; the core runtime should see
normal adapter events, queued outbound messages, and wakeup turns.
