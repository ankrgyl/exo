# IRC Adapter

The IRC adapter is a built-in Exo adapter implemented as a TypeScript worker. The host adapter runner supervises the worker, passes configuration through `EXO_ADAPTER_CONFIG`, receives JSONL events on stdout, and sends outbound messages by writing JSONL commands to stdin.

## How It Works

On startup, the worker opens a TCP or TLS socket to the configured IRC server, optionally sends `PASS`, then registers with `NICK` and `USER`. After the server sends welcome numeric `001`, the worker joins the configured channel.

The worker handles `PING` with `PONG`, parses `PRIVMSG` lines, and emits Exo message events when the configured trigger policy matches. Outbound `send_adapter_message` calls are converted into `PRIVMSG <channel> :<text>` on the existing IRC connection.

## Quick Start

For a fresh machine, use the canonical Exo installer. It clones the repo, asks
for local keys, writes `.env`, starts Docker-backed Exo, and configures the
default ExoChat control adapter:

```bash
mkdir exo && cd exo
curl -fsSL https://raw.githubusercontent.com/exoharness/exo/main/setup.sh -o setup.sh
bash setup.sh
```

For IRC plus Discord developer testing, run the developer canonical profile from
the repo after setup:

```bash
./exo.sh fresh --template dev
```

## Adapter Setup

Use the Exo setup flow:

```bash
./exo.sh fresh --pull-sandbox --setup irc
```

The setup prompt at `setup-prompt.md` asks Exo to create a built-in adapter similar to:

```json
{
  "name": "undernet-exo-test-plain",
  "source": "built_in",
  "config": {
    "type": "irc",
    "server": "irc.undernet.org",
    "port": 6667,
    "tls": false,
    "nick": "",
    "username": "",
    "realname": "Exo Test Bot",
    "channel": "#exo",
    "passwordSecretId": null,
    "trigger": "mention"
  }
}
```

Edit the nick and channel before using it on a public network. The default test channel and nick are only examples.

## Configuration

- `server`, `port`, and `tls` select the IRC endpoint.
- `nick`, `username`, and `realname` are used during IRC registration.
- `channel` must start with `#`.
- `passwordSecretId` can be used by the host-side config transform to inject `EXO_IRC_PASSWORD`; leave it `null` for unauthenticated networks.
- `trigger` is either `mention` or `all_messages`.

## Quirks And Gotchas

- IRC nicknames are global per network. If the adapter reports `Nickname is already in use`, pick a different nick or stop the old runner.
- With `trigger: "mention"`, the bot only wakes Exo when a channel message mentions the bot nick.
- IRC has limited formatting and no rich document support. Treat it as a reliable control channel, not a rich UI.
- The worker exits on socket close so the host runner can restart it.
- Outbound messages always go to the configured channel; the `target` from `send_adapter_message` is not required by this worker.
