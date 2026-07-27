# Agent CLI Adapter

The agent-cli adapter makes Exo callable from the shell like any other program, with read-write access to the directory you run it from:

```bash
cd ~/projects/some-repo
exo-cli "can you please set up this directory with a simple node environment?"
```

It is a built-in adapter implemented as a TypeScript worker. The host adapter runner supervises the worker, passes configuration through `EXO_ADAPTER_CONFIG`, receives JSONL events on stdout, and sends outbound messages by writing JSONL commands to stdin — exactly like the IRC adapter, except the "network" is a local unix socket.

## How It Works

1. The worker listens on a unix socket (default `~/.exo/agent-cli.sock`).
2. The `exo-cli` client connects, sends one JSON line `{ "cwd": "<pwd>", "prompt": "<text>" }`, and waits.
3. The worker translates the host `cwd` into the sandbox workspace mount (`mountRoot` → `mountPath`) and emits a message event whose text tells the agent which sandbox directory to `cd` into before working. Each client connection gets a unique `target`.
4. The agent works on the real files through the sandbox bind mount, then replies with `send_adapter_message` using that target. The worker routes the reply back over the socket and `exo-cli` prints it.

If you invoke `exo-cli` from a directory outside `mountRoot`, the message tells the agent it has no file access there, so it can reply accordingly instead of guessing.

## Setup

Put the client on your PATH:

```bash
ln -s "$PWD/exo/scripts/exo-cli" ~/bin/exo-cli
```

That is the only manual step. As long as the Exo stack is running (`./exo.sh`), the first `exo-cli` invocation bootstraps everything else automatically: it adds the workspace mount (default `$HOME/projects` → `/agent-cli`, override with `EXO_AGENT_CLI_ROOT` / `EXO_AGENT_CLI_MOUNT`), sends the agent a message asking it to create the adapter, waits for the worker socket to appear, then delivers your prompt. Subsequent invocations skip straight to the socket.

The adapter config the agent creates looks like:

```json
{
  "name": "agent-cli",
  "source": "built_in",
  "config": {
    "type": "agent-cli",
    "socketPath": null,
    "mountRoot": "/Users/you/projects",
    "mountPath": null
  }
}
```

You can also configure things explicitly at stack startup instead of relying on the bootstrap:

```bash
./exo.sh \
  --agent-cli-mount "$HOME/projects" \
  --setup agent-cli
```

## Configuration

- `socketPath`: host unix socket the worker listens on. `null` means `~/.exo/agent-cli.sock`. Override the client side with `EXO_AGENT_CLI_SOCKET`.
- `mountRoot`: absolute host directory that is bind-mounted into the agent sandbox. Must match the conversation mount's host path.
- `mountPath`: where `mountRoot` appears inside the sandbox. `null` means `/agent-cli`. Must match the conversation mount's sandbox path.

## Quirks And Gotchas

- The bootstrap requires the adapter runner to already be running; `exo-cli` cannot start the Exo stack itself and will tell you to run `./exo.sh` if it is down.
- The mount is part of the sandbox spec, not the adapter. The bootstrap adds it for you, but an already-running sandbox only picks it up when the sandbox is recreated.
- `exo-cli` exits after the first reply. The conversation keeps its history, so a follow-up `exo-cli` invocation continues the same conversation context.
- Replies can take as long as an agent turn. The client waits up to `EXO_AGENT_CLI_TIMEOUT_MS` (default 15 minutes).
- If the client disconnects (Ctrl-C) before the agent replies, the reply is nacked and surfaced as an adapter error event rather than delivered.
- The socket is created with mode 600; anyone with access to your user account can talk to the agent through it.
- Attachments are not supported over this adapter.
