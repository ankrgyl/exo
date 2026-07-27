# WhatsApp Adapter

The WhatsApp adapter is an experimental Exo library adapter implemented as a TypeScript worker using Baileys. It runs as a linked-device client: WhatsApp remains owned by the phone account, and Exo connects as an additional device after QR or pairing-code linking.

## How It Works

The host adapter runner starts `worker.ts` and passes adapter configuration through `EXO_ADAPTER_CONFIG`. The worker stores Baileys auth state on disk, opens a WhatsApp socket, emits JSONL events on stdout, and receives outbound send commands on stdin.

When WhatsApp asks for pairing, the worker logs an ASCII QR code and emits a lifecycle event named `qr`, or requests and emits a `pairing_code` when configured for pairing-code linking. After pairing, incoming text messages become Exo adapter message events. Outbound `send_adapter_message` calls send text, or text plus rich attachments, to the target WhatsApp chat id.

## Setup

Use the Exo setup flow:

```bash
./exo.sh fresh --pull-sandbox --setup whatsapp
```

The script watches `.exo/exo-adapters.log`, prints the QR code if it appears, and pauses while you scan it. Scan from WhatsApp using the linked-device flow.

The setup prompt at `setup-prompt.md` asks Exo to create a library adapter similar to:

```json
{
  "name": "whatsapp-dev",
  "source": "library",
  "config": {
    "type": "whatsapp",
    "authDir": null,
    "linkMethod": "qr",
    "phoneNumber": null,
    "trigger": "all_messages",
    "allowedChats": null
  }
}
```

## Configuration

- `authDir` controls where Baileys stores linked-device credentials. If omitted, the worker uses `.exo/adapters/whatsapp/<adapter-id>/auth` or the host-provided adapter state directory.
- `linkMethod` is `qr` or `pairing-code`. Use `pairing-code` with `phoneNumber` when QR linking is unreliable.
- `trigger` is `all_messages` or `contacts_only`.
- `allowedChats` can restrict wakeups to specific WhatsApp chat ids.

## Rich Outbound Content

The WhatsApp worker supports outbound image, video, audio, and document attachments. Use the `attachments` field on `send_adapter_message`; each attachment must specify exactly one of HTTPS `url`, base64 `data`, or `sandboxPath`.

Example image send:

```json
{
  "adapterId": "adapter-id",
  "target": "120363426815150953@g.us",
  "text": "Here is the chart.",
  "attachments": [
    {
      "kind": "image",
      "url": "https://example.com/chart.png",
      "data": null,
      "sandboxPath": null,
      "mimeType": "image/png",
      "fileName": null
    }
  ]
}
```

Documents require `mimeType` and `fileName`:

```json
{
  "kind": "document",
  "url": null,
  "data": "base64-pdf-bytes",
  "sandboxPath": null,
  "mimeType": "application/pdf",
  "fileName": "report.pdf"
}
```

If the image was created inside the agent sandbox, the adapter worker cannot read that sandbox path directly. Pass the file as `sandboxPath`; the host tool will stage it into `.exo/adapters/media` and send that staged host path to the worker:

```json
{
  "kind": "image",
  "url": null,
  "data": null,
  "sandboxPath": "/tmp/exo_media/funny-cat.jpg",
  "mimeType": "image/png",
  "fileName": null
}
```

Use `data` only for small inline payloads. Large inline base64 is rejected by the host tool.

For image, video, and document attachments, `text` is sent as the caption on the first caption-capable attachment. Audio attachments are sent as media, followed by a separate text message when needed.

## Quirks And Gotchas

- Baileys is an unofficial WhatsApp Web client library. It works for testing, but it is inherently more brittle than a supported API.
- If another WhatsApp Web session replaces this device, the worker can disconnect with a conflict/replaced message. Re-pair if needed.
- If the session is logged out, delete the adapter auth directory and pair again.
- WhatsApp sends require the target chat id from the inbound wakeup. Do not guess a phone number as the target.
- Inbound media is not downloaded yet. The worker currently exposes inbound text plus captions on image/video messages.
- QR codes and chat ids may appear in `.exo/exo-adapters.log`; treat that log as sensitive local state.
