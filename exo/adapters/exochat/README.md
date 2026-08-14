# ExoChat Adapter

The ExoChat adapter connects Exo to the lightweight WebSocket relay under
`website/`. Both the browser and adapter connect outbound to the public relay,
so setup requires no phone number, bot token, or third-party workspace.

## Setup

Use the default canonical setup:

```bash
./exo.sh fresh
```

The control script starts the adapter runner, watches `.exo/exo-adapters.log`,
and prints an ExoChat URL. You can talk to the agent either in the terminal UI
(`./exo.sh` REPL) or via that URL in a browser/phone — same conversation.
ExoChat keeps working after you `/exit` or close the terminal chat, as long as
the agent/adapter runner is still up. If you lose the URL, ask the agent for
it from the CLI.

For local relay testing, start the worker in one terminal:

```bash
pnpm dlx wrangler dev --config website/wrangler.jsonc --port 8787
```

Then start Exo against it from the repo root:

```bash
EXO_CHAT_BASE_URL=http://127.0.0.1:8787 ./exo.sh fresh
```

The setup prompt creates a library adapter similar to:

```json
{
  "name": "exochat",
  "source": "library",
  "config": {
    "type": "exochat",
    "baseUrl": null,
    "channelId": null,
    "secretId": null
  }
}
```

`baseUrl: null` uses the default hosted ExoChat relay. With both `channelId`
and `secretId` null, Exo generates a channel and stores its secret in the host
secret store. To reconnect to an existing channel, provide both values.

## Content

ExoChat is text-only for now. Inbound browser messages wake Exo as normal
adapter messages, and outbound `send_adapter_message` sends plain text back to
the browser. Attachments are rejected so the hosted relay stays cheap and
predictable. Larger files and rich content can use a later WebRTC/direct path or
object storage backed upload/download endpoints.
