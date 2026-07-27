---
title: A Sandboxed Conversation
description: Give your agent a shell in an isolated sandbox.
---

# A Sandboxed Conversation

The default REPL conversation is plain chat. To let the agent run shell
commands, create an agent and a conversation explicitly — conversations can
own a sandbox.

## Create an agent and conversation

```bash
exo agent create --model gpt-5.5 "Sandbox Example"
exo conversation create sandbox-example "Local Dev"
exo repl --agent sandbox-example --conversation local-dev
```

The agent can now execute commands in the conversation's sandbox via the
shell tool.

## Choosing a sandbox backend

Local backends are selected with `--sandbox-backend` (or
`EXO_SANDBOX_BACKEND`):

| Backend | Isolation | Notes |
|:--------|:----------|:------|
| `docker` | Container | Default choice; requires Docker |
| `apple-container` | Container | macOS |
| `local-process` | **None** | Runs directly on the host |

::: warning
  `local-process` gives the model unrestricted shell access to your machine.
  Use it only when you trust the agent and the task.
:::

Remote sandbox providers (Daytona, E2B, Vercel, Sprites, Tensorlake, AWS
AgentCore) are
configured as *provider bindings*:

```bash
exo secret set daytona --env DAYTONA_API_KEY
exo provider configure --provider daytona --secret daytona
```

## Sandbox scope and image

`conversation create` accepts:

- `--sandbox-scope <agent|conversation>` — whether the sandbox is shared by
  all of the agent's conversations or owned by this one.
- `--sandbox-image <image>` — the container image to boot.

You can also run one-off commands in a conversation's sandbox from the CLI:

```bash
exo conversation sandbox run sandbox-example local-dev "ls /"
```

Sandboxes can be snapshotted and rewound together with conversation history
— see [Time Travel](../concepts/time-travel).
