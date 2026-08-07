# Signal Adapter

The Signal adapter is an experimental Exo library adapter implemented as a TypeScript worker around `signal-cli`. It assumes you already have a real Signal account on a phone, have set a Signal username, and want Exo to run as a linked device.

## How It Works

The host adapter runner starts `worker.ts` and passes adapter configuration through `EXO_ADAPTER_CONFIG`. The worker stores `signal-cli` state on disk, discovers or links a local account, starts `signal-cli jsonRpc --receive-mode=on-connection` for inbound messages, emits JSONL events on stdout, and receives outbound send commands on stdin.

If `account` is `null`, the worker first runs `signal-cli listAccounts`. If an account already exists in the configured state directory, it reuses it. Otherwise it runs `signal-cli link -n <deviceName>`, prints a QR code, waits for linking to complete, discovers the account, and then starts JSON-RPC receive mode.

Incoming Signal messages become Exo adapter message events. Outbound `send_adapter_message` calls send through the same `signal-cli jsonRpc` process with either a recipient or a group id, depending on the target format.

## Setup

Install the JVM `signal-cli` distribution and Java, then make sure the `signal-cli` script and Java are available to the adapter worker. Run setup with:

```bash
./exo.sh fresh --pull-sandbox --setup signal
```

The script watches `.exo/exo-adapters.log`, prints the linked-device QR code if it appears, and pauses while you scan it from Signal: Settings > Linked devices > Link new device.

The setup prompt at `setup-prompt.md` asks Exo to create a library adapter similar to:

```json
{
  "name": "signal-dev",
  "source": "library",
  "config": {
    "type": "signal",
    "account": null,
    "deviceName": "Exo",
    "configDir": null,
    "trigger": "all_messages",
    "allowedContacts": null
  }
}
```

## Configuration

- `account` is the local Signal account identifier for `signal-cli -a`. Use `null` for first-time setup or automatic account discovery.
- `deviceName` is shown in Signal's linked-device list during pairing.
- `configDir` controls where `signal-cli` stores linked-device state. If omitted, the worker uses `.exo/adapters/signal/<adapter-id>/signal-cli` or the host-provided adapter state directory.
- `trigger` is `all_messages` or `contacts_only`.
- `allowedContacts` can restrict wakeups to specific sender identifiers.

## Installing On Mac

Install Java with Homebrew:

```bash
brew install openjdk
```

The native/Homebrew `signal-cli` binary may receive messages but fail outbound sends with `NETWORK_FAILURE` and an error mentioning `IdentityKeyDeserializer has no default (no arg) constructor`. Use the JVM `signal-cli` distribution for Exo instead.

One tested local setup uses a wrapper or symlink named `signal-cli` on `PATH`, backed by the JVM distribution.

Java must be visible on `PATH` for the JVM script. On Homebrew macOS, prefix Exo commands with:

```bash
PATH="/opt/homebrew/opt/openjdk/bin:$PATH" \
./exo.sh fresh --pull-sandbox --setup signal
```

You can also add `/opt/homebrew/opt/openjdk/bin` to your shell profile.

## Targets

Signal outbound sends require a target. Use the `target` from the inbound wakeup when replying. For direct messages, supported target forms include:

- Signal usernames with the `u:` prefix, such as `u:example.01`.
- Phone numbers, such as `+16505551212`.
- Signal UUIDs from inbound events.
- `ACI:` or `PNI:` identifiers.

Long opaque non-recipient strings are treated as group ids.

## Rich Outbound Content

The Signal worker supports outbound attachments through `signal-cli` JSON-RPC. Use the shared `attachments` field on `send_adapter_message`.

For sandbox-generated files, prefer `sandboxPath`; the host tool stages the file and passes a local path to `signal-cli`:

```json
{
  "kind": "image",
  "url": null,
  "data": null,
  "sandboxPath": "/tmp/exo_media/image.png",
  "mimeType": "image/png",
  "fileName": "image.png"
}
```

Signal attachments can also use HTTPS `url` or small inline `data` payloads. The host tool validates and stages all attachment sources into `.exo/adapters/media` before passing them to `signal-cli`.

## Quirks And Gotchas

- If linking succeeds but later setup keeps asking for QR codes, check that the same `configDir` is being reused.
- Signal group support depends on targets surfaced by incoming group messages. Prefer replying to the inbound target instead of inventing group ids.
- QR codes, account ids, phone numbers, and message routing metadata may appear in `.exo/exo-adapters.log`; treat that log as sensitive local state.
