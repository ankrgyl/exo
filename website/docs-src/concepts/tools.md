---
title: Tools
description: Trusted functions the harness exposes to the model during a turn.
---

# Tools

A **tool** is a function the model calls during an active turn. The harness
validates its arguments, executes it, and records durable `tool_requested` and
`tool_result` events. Large results are stored as artifacts with a compact
preview in model context.

Tools are an executor concept. Exoharness supplies durable events, artifacts,
bindings, secrets, and sandbox processes; the TypeScript harness decides which
tools the model sees and how their handlers run.

## Trust model

TypeScript tool modules run as trusted code in the harness process. Loading one
is a trust decision. Commands that a handler starts inside the conversation
sandbox still use that sandbox's policy, but there is not yet a capability
sandbox around the tool module itself.

Credentials belong in Exoharness secrets. Tool configuration refers to a
secret; definitions, prompts, and results must not contain the raw value. For
installed tools, an initialization value of exactly `${ENV_VAR}` is resolved
from the host environment each time the tool loads, so the raw value never
enters the lockfile.

## Local tool sources

Tools can be installed from a workspace-relative directory or a Git repository
pinned to an immutable commit. Both may select a contained subdirectory. Exo
copies the selected source into its managed `.exo/tools/` store and records it
in a small lockfile.

Each source contains an `exo-tool.json` with exactly `schemaVersion`, `id`, and
`module`. The TypeScript module owns its model-facing name, description, input
and output schemas, handler, and initialization contract.

Argument schemas must satisfy the model API's strict mode: every key in
`properties` also listed in `required`, optional parameters typed as nullable
(for example `{"type": ["string", "null"]}`), and `additionalProperties: false`
at every object level. Non-conforming installs are rejected, and a
non-conforming installed tool is skipped at registration instead of breaking
the model call for every turn.

There is one workspace-local registry and no agent or conversation tool scope.
Each `tools.lock.json` tool entry contains only `id`, `source`,
`initialization`, and `installPath`. Malformed lockfiles fail clearly. Broken
installed modules are logged and skipped without persistent audit or
quarantine state, and failed installs clean their staging data.

Configured library modules remain supported. The legacy `install_agent_tool`
and `uninstall_agent_tool` tools and `.exo/agent-tools/` directory are opt-in
compatibility paths through `enable_agent_tool_creation`, which defaults to
`false`.

## Manage and inspect

`manage_tool` is the only write surface. It can install or remove tools.
Install is an upsert by stable manifest id and accepts only workspace-relative
directories or exact pinned Git commits, with optional contained
subdirectories. For a tool created with sandbox `shell`, write under
`/workspace/exo/.exo/tool-sources/<name>` and install the relative path
`.exo/tool-sources/<name>`. Absolute sandbox paths are rejected because
`manage_tool` runs in the host harness. Changes are available on the next model
round.

`inspect_tools` is read-only and supports `list` and `get` for active or
installed tools. The operator CLI is also read-only:

```text
exo tools list
exo tools get <id>
```

## Bootstrap and profiles

The bootstrap profile has four tools:

```text
shell
inspect_tools
manage_tool
rebuild_and_restart_exo
```

The practical profile adds the shipped scheduler, adapter, sandbox recovery,
introspection, memory, todo, skill, and web tools. Bootstrap and practical are
the only profiles.

Code mode, generic adapter command generation, and treating the scheduler as an
adapter are also deferred. Scheduling continues through the current scheduler
service.

## Web search providers

The practical profile's `web_search` uses Brave when a `brave-api-key` secret
or `BRAVE_API_KEY` environment variable is configured, otherwise DuckDuckGo.
Set `EXO_WEB_SEARCH_PROVIDER` on the host to explicitly select `brave`,
`duckduckgo`, or `parallel`. For example, start Exo with:

```bash
EXO_WEB_SEARCH_PROVIDER=parallel ./exo.sh
```

The `parallel` option uses [Parallel Search MCP](https://docs.parallel.ai/integrations/mcp/search-mcp)
at `https://search.parallel.ai/mcp` over Streamable HTTP. It needs no Parallel
account or API key. Free access is rate limited; errors are returned without
switching providers. Unset the variable to restore automatic Brave/DuckDuckGo
selection. Restart an already-running executor with the new environment.

Once selected, the agent can call `web_search` during its turns. Each uncached
call sends its query (also used as the search objective) and conversation ID
to Parallel from the host, even if sandbox networking is disabled. Do not put
private information in queries. See Parallel's [terms](https://parallel.ai/customer-terms)
and [privacy policy](https://parallel.ai/privacy-policy).

Queries can contain up to 200 characters. `count` caps returned results locally
at 1-10, with a default of 5; it does not control how much Parallel retrieves.
Results keep their source URLs, titles, and excerpt snippets. Snippets are
limited to 2,500 characters each, with `truncated` indicating shortening;
service warnings are preserved. Requests have a 12-second deadline and a 1 MB
response limit. The existing 15-minute search cache still applies.

This option changes only search. `web_fetch` still fetches pages directly from
the host with Exo's existing private-address guards; it does not use Parallel.
The bootstrap profile does not gain web tools.

See [Adapters](./adapters) for the long-running integrations that can wake a
conversation.
