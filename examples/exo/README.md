# Exo Harness

Exo is a persistent agent built on Exo designed to be helpful wherever
there is a task to do from a computer. It supports task scheduling, a full
sandbox where it can install its own tools and integrations, and right now
supports ExoChat, IRC, WhatsApp, Signal, Discord, and a shell CLI (`exo-cli`) out of
the box.

## Quickstart

The simplest path from a fresh checkout to a running Exo.

**Prerequisites:** Docker running, a Rust toolchain, and Node with pnpm.

1. Install JS dependencies and configure your model key:

```bash
pnpm install
cp .env.example .env   # then fill in OPENAI_API_KEY
```

2. Tell Exo who you are (optional but recommended):

```bash
./exo.sh setup-profile
```

This interactively asks for your name and any local instructions, and writes
them to `.exo/exo-profile.md` (git-ignored). The harness loads it as an
extra prompt on every turn, so the agent greets you by name from its first
reply. You can rerun this or edit the file directly at any time.

3. Build and start everything with one command (run from the repo root):

```bash
./exo.sh fresh
```

This builds the `exo` binary, creates the agent and a `dev` conversation, pulls
the Docker sandbox image, mounts this repo at `/workspace/exo`, starts the
scheduler and adapter runner, sets up the ExoChat adapter, prints a browser
chat URL using the hosted control plane at `https://exoharness.ai`, and drops
you into a REPL.

For developer testing with IRC and Discord instead, use the `dev` template:

```bash
./exo.sh fresh --template dev
```

4. Chat with Exo in the REPL:

```text
dev> hello! what can you do?
```

5. Talk to it from any shell. Put the CLI on your PATH once:

```bash
ln -s "$PWD/examples/exo/scripts/exo-cli" ~/bin/exo-cli
```

Then from any directory under `~/projects`:

```bash
cd ~/projects/some-repo
exo-cli "set up a simple node environment in this directory"
```

The agent has read-write access to the directory you call it from and replies
to your terminal when it finishes.

Notes:

- `fresh` deletes existing agents, conversations, and adapters first. For
  day-to-day restarts that keep state, drop `fresh`:
  `./exo.sh`
- For a minimal start without Docker defaults or adapter setup:
  `./exo.sh --template minimal --pull-sandbox`

## Setting up Nostr chat

Exo can join a public NIP-29 group through the library Nostr chat adapter:

```bash
./exo.sh --setup nostr-chat
```

The default setup joins `openagents-public` at
`wss://relay.openagents.com`. Pass another relay URL and group ID in the
adapter configuration to use any compatible NIP-29 deployment. Exo generates
and protects a new adapter key unless the configuration names an existing Exo
secret.

See [`adapters/nostr-chat/README.md`](adapters/nostr-chat/README.md) for the
protocol, identity, and trigger details.

## Setting up Discord

Exo can connect to Discord through the library Discord adapter. The short
path is:

1. Create a Discord application and bot in the
   [Discord Developer Portal](https://discord.com/developers/applications).
2. In the bot settings, enable **Message Content Intent**.
3. Invite the bot to your server with at least these bot permissions:
   **View Channels**, **Send Messages**, **Read Message History**, and
   **Attach Files** if you want Exo to send images or other attachments.
4. Store the bot token as the secret expected by the setup prompt:

   ```bash
   export DISCORD_BOT_TOKEN="..."
   ./target/debug/exo secret set discord-bot-token --env DISCORD_BOT_TOKEN
   ```

5. Create or confirm the adapter:

   ```bash
   ./exo.sh --setup discord
   ```

   If you are starting from scratch for adapter development, you can include
   Discord in the developer canonical setup:

   ```bash
   ./exo.sh fresh --template dev
   ```

6. Copy a Discord channel id for testing. In Discord, enable **User Settings** >
   **Advanced** > **Developer Mode**, then right-click the target channel and
   choose **Copy Channel ID**.

To test outbound messages, ask Exo:

```text
Send "hello from Exo" to Discord using adapter <adapter-id> and target <channel-id>.
```

To test inbound wakeups, send a normal message in a channel the bot can read.
The default setup uses `trigger: "all_messages"`, so the bot does not need to be
mentioned. Discord attachments are forwarded too: inbound images are attached to
the model wakeup for analysis, and outbound files can be sent with
`send_adapter_message` attachments.

For voice chat, richer attachment examples, and the full configuration surface,
see [`adapters/discord/README.md`](./adapters/discord/README.md).

## Self Introspection

Exo starts with sandbox shell support by default. The startup script mounts
this repository into the sandbox at `/workspace/exo` and makes that path
available to the harness as `EXO_REPO`. The self map lives at:

```text
/workspace/exo/examples/exo/SELF.md
```

The checked-in source for that map is `examples/exo/SELF.md`. It points
Exo to the harness, prompts, adapter runtime, scheduler, sandbox tools, and
service guardian. Use `--self-repo-mount <path>` or `EXO_REPO` to choose a
different sandbox mount path.

## Service Guardian

`examples/exo/scripts/exo-service-guardian` is a host-side helper for
self-maintenance. It owns build and service-control actions that should happen
outside the agent's sandbox, while preserving `.exo` state such as adapter
pairing data, conversations, artifacts, and sandbox records.

Common commands:

```bash
examples/exo/scripts/exo-service-guardian status
examples/exo/scripts/exo-service-guardian build
examples/exo/scripts/exo-service-guardian restart-adapters
examples/exo/scripts/exo-service-guardian restart-scheduler
examples/exo/scripts/exo-service-guardian restart-all --build
```

Save local launch settings for later restarts with:

```bash
examples/exo/scripts/exo-service-guardian configure --sandbox-backend docker
```

The service guardian manages only the scheduler and adapter runners. Start or
reconnect an interactive REPL with `./exo.sh`.

Exo can call the same host-side surface through the `guardian_action` tool.
That tool exposes only allowlisted actions such as `status`, `build`,
`restart_adapters`, `restart_scheduler`, `restart_all`, and `logs`.
Restart actions are handed off to a detached guardian process after a short
delay so the current agent turn can finish before services stop. Detached
restart output is written to `.exo/exo-service-guardian-actions.log`.

When `./exo.sh --control` is running, it also acts
as the foreground REPL supervisor. Guardian builds write
`.exo/exo-control.restart`; the control wrapper sees that marker, restarts only
the child `exo repl`, and keeps your terminal open.

## Setting up the identity

`examples/exo/prompts/me.md` is the committed, generic Exo identity
prompt. It's best to keep this high level, and not specific to any given
instance or the local deployment environment.

Use `.exo/exo-profile.md` for local instructions instead. The harness loads
it as an additional developer prompt when it exists, and `.exo` is ignored by
git. To create it interactively:

```bash
./exo.sh setup-profile
```

The script asks for the user's name and any extra local instructions. To use a
different local prompt path, set `EXO_LOCAL_PROMPT_FILE` or pass
`--local-prompt-file <path>`.

## Tools

Exo includes the normal minimal tools:

- `shell`
- `install_agent_tool` when agent tool creation is enabled
- configured library tools

It also adds scheduler tools:

- `schedule_sandbox_task`
- `list_scheduled_tasks`
- `cancel_scheduled_task`
- `delete_scheduled_task`

`cancel_scheduled_task` disables a task and preserves its record/history.
`delete_scheduled_task` removes the task record and stored run history.

And adapter tools:

- `create_adapter`
- `list_adapters`
- `disable_adapter`
- `delete_adapter`
- `send_adapter_message`

`disable_adapter` stops future adapter wake-ups while preserving the adapter
record and event history. `delete_adapter` removes the adapter record and stored
events.

And skill tools (see `docs/design/skills-arch.md` for the design):

- `install_skill`
- `list_skills`
- `use_skill`
- `read_skill_file`
- `uninstall_skill`

Skills follow the standard agent-skills format (a `SKILL.md` with `name` and
`description` frontmatter plus markdown instructions, optionally bundling text
files), so skills published for Claude Code, OpenClaw, or Hermes install
unchanged. They are stored as agent artifacts, persist across conversations,
and survive sandbox rewinds. Each turn the agent sees only installed skill
names and descriptions; it loads a skill's full instructions with `use_skill`
when a task matches.

## Adapters

Adapters are host-owned long-running runtimes for external applications. They
are intentionally separate from scheduled sandbox commands: adapters own sockets,
reconnect behavior, inbound message parsing, event history, and conversation
wake-ups. Agents configure adapters with tools, and the local adapter runner
started by `./exo.sh` keeps them connected.

Exo ships with ExoChat, Nostr chat, IRC, WhatsApp, Signal, Discord, and
agent-cli adapters (see `adapters/agent-cli/README.md` for the shell CLI). The
default canonical setup turns on ExoChat:

```bash
./exo.sh
```

For developer testing with IRC and Discord, use:

```bash
./exo.sh --template dev
```

To send every setup prompt before opening the REPL:

```bash
./exo.sh --setup-all
```

For a fresh control agent with a local profile prompt and all adapters:

```bash
PATH="/opt/homebrew/opt/openjdk/bin:$PATH" \
  ./exo.sh fresh \
  --agent exo-agent \
  --agent-name "Exo" \
  --conversation dev \
  --setup-profile \
  --setup-all
```

This will:

- Prompt you for any needed adapter configuration (such as nicknames, channel/server info for IRC, or pairing for WhatsApp/Signal).
- Write adapter configuration to `.exo/adapters/`.
- Start the background adapter runner so the agent can receive and react to external messages in real time.

The adapter runner starts by default. Use `--no-adapters` to skip it, or
`--adapters` to force it on when an environment override disabled it.

You can list configured adapters with:

```bash
target/debug/exo --harness exo adapters list
```

See the sections below for more details on individual adapter configuration.

### IRC

The first built-in adapter is IRC. It runs as a host-supervised Node.js worker
that connects to a configured server/channel, responds to `PING`, parses
`PRIVMSG`, and wakes the conversation when the trigger policy matches. The
recommended trigger is `mention`, which only wakes the conversation when a
channel message mentions the adapter nick. `all_messages` is available for
quieter channels.

### WhatsApp

Exo also includes an experimental WhatsApp adapter using Baileys. The Rust
adapter runtime owns supervision, durable events, conversation wakeups, and
outbox draining; workers own protocol-specific sockets and parsing. When first
run, the WhatsApp worker emits a QR pairing event into adapter history and logs;
after pairing, Baileys auth state is stored under
`.exo/adapters/whatsapp/<adapter-id>/auth` by default.

### Signal

The Signal adapter uses `signal-cli` as a linked device. If its `account` config
is null, the worker starts `signal-cli link`, logs a QR code for the phone's
linked-device flow, discovers the linked account with `signal-cli listAccounts`,
then runs `signal-cli -a <account> jsonRpc`. Outbound DM targets should be
Signal usernames with the `u:` prefix, such as `u:example.01`, unless an inbound
wakeup provides a more precise target.

## Sandbox Modes

Exo conversations default to `sandboxScope: "agent"`. The `shell` tool uses
the sticky agent sandbox, so packages installed through the Exo REPL, such as
`curl` or `python3`, are available to scheduled task runs and future
conversations for the same agent while that warm sandbox is still alive. Normal
REPL exits leave the warm sandbox running so the next Exo process can
reattach to it.

Because exo defaults to agent scope, you don't need to specify anything from
the cli. The following command will create a REPL with the agent and a
persistent sandbox that will be durable across conversations

```bash
./exo.sh --pull-sandbox
```

If you want a conversation to have its own sandbox, use `sandboxScope: "conversation"`:

```bash
./exo.sh --conversation isolated-dev --sandbox-scope conversation
exo --harness exo conversation update exo-agent isolated-dev --sandbox-scope conversation
```

Scheduled tasks also default to `sandboxMode: "agent"`. A task can explicitly use
`sandboxMode: "conversation"` to run in the current conversation's sandbox, or
`sandboxMode: "task_fresh"` to create a separate task-owned sandbox.

Important limitation: the current sandbox filesystem is not durable across warm
container death. Exo stores a durable pointer to the agent's sandbox, but
package installs made interactively live in the running warm container. If the
host restarts or the container backend cleans up the warm container, a later
scheduled task may recreate the sandbox from the base image and lose packages
installed with commands like `apt-get install python3`. Stale warm containers are
eligible for orphan cleanup after roughly 24 hours.

For reliable scheduled tasks, prefer one of these:

- Use a sandbox image that already contains required dependencies.
- Include a `setupCommand` that installs required packages before the task runs.
- Keep task code/data on mounted storage instead of relying on mutated container
  filesystem state.

The task-owned sandbox starts from the configured image and mounts. It is reused
across the task's runs and stopped when the task is cancelled.

The current scope model is Exo-specific policy on top of conversation-owned
exoharness sandbox records. The default mental model is agent-scoped, while
conversation and task scopes remain available for isolation.
