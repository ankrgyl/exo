# Tools and Adapters

This document defines the boundary between tools and adapters in Exo and
specifies the recommended architecture for each. It is written as a design
reference for building the system.

## Overview

A **tool** is a model-invoked request/response operation. During an active
agent turn, the model supplies structured arguments, the harness executes the
operation, and a structured result is returned to the same turn.

An **adapter** is a supervised integration with an external event source or
destination. It remains available while the agent is idle, maintains the
state needed to reconnect and resume, accepts asynchronous events, and can
wake the agent by creating a new turn.

The shortest useful distinction:

- A tool lets the agent call the outside world.
- An adapter lets the outside world call the agent.

Both are needed because an autonomous agent must be able to act (tools) and be
acted upon (adapters). Tools alone cannot wake an idle agent; adapters alone
cannot express a bounded, model-initiated operation with a result the model
waits on. The two compose: adapters wake the agent with events, and the agent
responds through tools — including tools that enqueue outbound work back
through an adapter.

The durable architectural rule:

> Tools are bounded commands executed on behalf of an active turn. Adapters
> are supervised event bridges that remain available between turns and can
> create new turns.

## Tools

### Conceptual model

A tool behaves like a function call from the model's perspective:

1. The model requests the tool during a turn, with a unique call id and
   structured arguments.
2. The harness validates, then executes or rejects the request.
3. The request and result are durably recorded.
4. The result is available to the next model round in the same turn.

The implementation may be asynchronous internally, but its conversational
semantics are synchronous: the model waits for the result before deciding what
to do next. A tool never independently wakes the agent later.

### State

Tools are **invocation-scoped**, which constrains where state lives, not
whether state exists:

- **Invocation state** — temporary values for one call; discarded when the
  call finishes.
- **Configuration** — stable initialization data (endpoint, workspace path,
  secret reference) attached to the tool registration.
- **Domain state** — durable state owned by another subsystem (files,
  artifacts, a database, a sandbox, an adapter store) that a tool may
  explicitly read or modify.

What a tool must avoid is hidden, process-local session state whose meaning
depends on earlier calls. That state cannot be replayed, inspected, or
recovered after a restart. If later calls need earlier state, represent it
explicitly with an identifier or store it in a durable subsystem:

- `read_file(path)` is naturally stateless.
- `start_process(command)` creates durable process state and returns a process
  id; later calls reference the id.
- `send_adapter_message(adapter_id, target, text)` appends to a durable
  adapter outbox and returns after enqueueing; the adapter owns delivery.

### Tool contract

Each tool definition includes:

- A stable, model-facing name.
- A description of when to use it, and its input/output schemas.
- Its provenance: built-in, standard-library, third-party, or agent-authored.
- Timeout and cancellation behavior.
- Idempotency and side-effect semantics.
- Declared capabilities (network, filesystem, sandbox, subprocesses, secrets,
  external side effects) and required secret **names**, never values.

Each invocation has:

- A unique call id and the agent/conversation/turn/sandbox context.
- A bounded execution lifetime.
- A durable request and result record.
- Artifact references for output too large for model context.
- Structured errors rather than unrecorded exceptions.

Side effects do not disqualify an operation from being a tool. The defining
property is that the agent initiates a bounded request during a turn and
receives a result or acknowledgement.

### Provenance categories

- **Built-in** — shipped with the minimal Exo runtime, versioned and reviewed
  with Exo itself. Cannot be independently installed or removed.
- **Standard-library** — optional tools published and signed by the core Exo
  team (web search, browser control, GitHub, etc.). Released independently of
  the runtime; must declare capabilities and be explicitly installed.
- **Third-party** — community or vendor tools using the same registry and
  execution contracts. Installation never implies trust: Exo shows publisher,
  requested capabilities, secret requirements, signature, and review status
  before enabling. They run with only their declared capabilities.
- **Agent-authored** — created by an agent for its own work. Local, unsigned,
  and scoped to an agent, conversation, or workspace by default. Validated
  against the same contract, run with the narrowest capabilities, never
  auto-published. Promotion to a signed registry release is a separate,
  reviewed action.

## Built-in Tools

A minimal Exo installation contains exactly four model-facing tools. The
harness itself (not a tool) provides the substrate: sending tool definitions
to the model, validating arguments, dispatching through a capability boundary,
recording durable request/result events, storing large results as artifacts,
and loading/refreshing installed tools. An agent cannot build that substrate
from `shell`, because the substrate is what turns code into a callable tool.

```text
shell
inspect_tools
manage_tool
rebuild_and_restart_exo
```

These form a bootstrap chain: inspect and build in the workspace (`shell`),
discover tools (`inspect_tools`), install them (`manage_tool`), and put
changes to Exo itself into use (`rebuild_and_restart_exo`). Everything else is
discoverable and installable on demand.

### `shell`

Runs commands inside the conversation sandbox. It is the universal workspace
primitive: inspecting files, editing code, running tests, composing programs,
and using core CLIs.

`shell` is constrained by the sandbox's filesystem, network, process, and
secret policy. It has no direct access to host-owned tool registrations,
adapter state, or runtime activation — those require the narrow,
authenticated tools below.

### `inspect_tools`

One read-only discovery interface across three sources:

```text
active       tools currently exposed to the model
installed    installed tools, including failed or out-of-scope registrations
registry     tools available from an operator-configured remote registry
```

```text
inspect_tools(
  source: active | installed | registry,
  operation: list | search | get,
  registry?: configured registry id,
  query?: text,
  tool_id?: stable tool id,
  version?: exact version,
  filters?: provenance/capability/compatibility filters,
  cursor?: opaque pagination cursor,
  limit?: bounded result count
)
```

The schema is a discriminated union: `search` requires `query`, `get` requires
`tool_id`, and registry arguments are rejected for local sources. `list` and
`search` return compact paginated summaries; `get` returns the full
definition, provenance, capabilities, configuration requirements, and load
errors.

Constraints:

- `registry` names an operator-configured registry; arbitrary URLs are
  rejected.
- Registry reads never execute tool code and have no installation side
  effects.
- `inspect_tools` is a read-only facade, not the authoritative registry. The
  harness maintains its own internal registration API (used to build the tool
  definitions sent to the model each round) and the host maintains the remote
  registry client. Both also back the `exo tools` / `exo registry` CLIs, so
  operators can enumerate and repair registrations even if `inspect_tools` is
  broken.

The model already receives names, descriptions, and schemas of callable tools
every round; `inspect_tools` supplies the operational and provenance detail
that should not consume prompt space.

### `manage_tool`

Owns the lifecycle of non-built-in tools through a discriminated action
schema:

```text
install    local source, or configured registry id + tool id + exact
           version/digest; config; scope
upgrade    installed tool id, configured registry id, exact target
           version/digest
remove     tool id
```

All mutations go through one host-owned lifecycle manager that validates the
tool contract, records provenance, applies capability and secret policy,
updates the lockfile, and refreshes the model's tool registry for the next
round.

Lifecycle rules:

- `manage_tool` requires the stable identity returned by
  `inspect_tools(operation = "get")` — never a search-result position.
- Built-in tools can be inspected but not upgraded or removed; they change
  only with Exo itself.
- Install registers and exposes the tool atomically to an explicit scope
  (agent, conversation, workspace, or host). A local tool never silently
  becomes global. There is no separate enable step.
- Upgrade installs the candidate side by side, presents the manifest and
  capability diff, validates, then switches atomically. New capabilities or
  secret access are never granted implicitly. A failed upgrade leaves the
  current version active.
- Remove unregisters the tool; stored source is garbage-collected when no
  longer referenced.
- Configuration is supplied at install time; changing it means upgrading with
  replacement configuration.
- Every transition produces a durable audit event with an idempotent
  operation id.

### `rebuild_and_restart_exo`

Puts changes to the Exo runtime itself into use through a fixed,
guardian-owned pipeline. It accepts no arbitrary build or restart command; the
agent's intent is "validate my changes and run them," not "operate a
deployment state machine."

The pipeline:

1. Snapshot the candidate source and record its revision/digest.
2. Build in an isolated directory using Exo's declared build recipe.
3. Run required tests and compatibility checks.
4. Stage binaries; record the currently active version.
5. Drain adapters and scheduled work where possible.
6. Write a durable reboot notice.
7. Atomically switch versions and restart managed services.
8. Run bounded health checks; roll back automatically on failure.
9. Wake the agent with the build, activation, or rollback outcome.

The call is asynchronous from the calling turn: it durably queues the update,
returns an update id, and lets the turn finish before the process stops. The
guardian and its update records live outside the replaceable runtime — the
active process is never responsible for proving its own restart succeeded.
Update requests are idempotent, versioned, serialized, and auditable. Whether
activation requires human approval is installation policy, but new
capabilities are never granted implicitly by a rebuild.

## Practical Tool Profile

The practical profile keeps the four built-in tools and preinstalls a small
set of Exo-maintained standard-library tools, plus two adapters (next
section). Additions are foundational primitives, not service integrations:
Discord and web search are still installed separately, on demand.

`shell` alone is insufficient for these because they mutate host-owned state
from inside the sandbox being managed; they need narrow, authenticated,
auditable host APIs.

```text
# Standard-library sandbox lifecycle and recovery
get_sandbox_status
snapshot_sandbox
list_sandbox_snapshots
rewind_sandbox

# Standard-library generic adapter control plane
create_adapter
list_adapters
enable_adapter
disable_adapter
delete_adapter
send_adapter_message

# Standard-library code mode
run_tool_program
```

### Sandbox lifecycle

Sandbox creation and attachment happen automatically when a conversation
starts, so they need no tools. Snapshot and rewind need host calls because a
sandbox cannot safely snapshot or rewind itself. If multiple sandboxes per
conversation arrive later, `create_sandbox` / `list_sandboxes` /
`select_sandbox` join this group without changing the principle.

### Adapter control plane

Generic lifecycle tools for the core adapter runtime — not protocol-specific
integrations. `create_adapter` references an installed adapter implementation
plus configuration and secret bindings. `send_adapter_message` is generic
because every adapter shares the durable outbox and authorization path; Exo
does not install a different send primitive per protocol. A fresh installation
exposes these tools with zero adapters installed.

### Code mode: `run_tool_program`

The model writes a small JavaScript/TypeScript program that invokes registered
tools, expressing loops, batching, retries, pagination, and data
transformation without a model round per mechanical operation:

```js
const repos = await tools.call("list_repositories", { organization: "exo" });
const results = [];
for (const repo of repos.value) {
  results.push(
    await tools.call("inspect_repository", { repository: repo.name }),
  );
}
return results;
```

Code mode is an orchestration frontend over the same tool registry, not a
bypass:

- The program runs outside the harness in a restricted sandbox: no host
  filesystem, environment, secrets, or network except through callable tools;
  bounded CPU, wall time, memory, output, concurrency, and tool-call count;
  cancelled with the parent turn; no detached work; no dynamic installation.
- Every nested `tools.call` is validated against the normal tool definition
  and executed with the same capability, approval, and secret policy as a
  direct model call, producing its own durable `tool_requested`/`tool_result`
  pair linked to the parent call id.
- The program is not transactional: the result reports completed and failed
  calls precisely, and automatic retries are safe only for tools declaring
  idempotency.
- Recursive `run_tool_program` is disallowed. High-risk tools (`manage_tool`,
  `rebuild_and_restart_exo`) are excluded by default.

Code mode is right when the control flow is mechanical. When each intermediate
result needs semantic judgment, the normal model/tool loop stays in control.

### Deliberately not tools

- **Secrets** — the agent can see which secret bindings exist and request
  missing ones (surfaced by `manage_tool` and `create_adapter`), but no tool
  returns raw secret values. The host resolves values only when launching an
  authorized tool or worker.
- **Artifacts** — tool results return readable artifact references; artifacts
  mount read-only into the sandbox. No separate artifact-management tools.
- **Runtime diagnostics and restart-without-rebuild** — available through the
  `exo` CLI via `shell`; not always-present model-facing tools.

## Adapters

### Conceptual model

An adapter is a durable bridge between an external event system and Exo:

```text
external service
    -> adapter receives event
    -> adapter normalizes and durably records event
    -> adapter routes event to a conversation
    -> Exo creates a wakeup turn
    -> agent decides whether and how to respond
    -> agent calls an outbound tool
    -> durable outbox
    -> adapter delivers response
```

An adapter is not a special kind of model turn; it is infrastructure that can
cause a normal turn to begin. An integration does not need a permanent socket
to be an adapter — a webhook receiver qualifies because it accepts events
while the agent is idle and can initiate turns.

Adapters are the right abstraction for chat systems, email, webhooks, message
queues and event streams, filesystem watchers, voice/telephony sessions, and
any subscription that emits events.

### Lifecycle

An adapter's lifecycle is independent of any turn:

```text
created -> enabled -> starting -> connected
                          ^          |
                          |          v
                       retrying <- disconnected

enabled/connected -> disabled -> deleted
```

The host supervisor — not an agent turn — starts workers, monitors health,
restarts after transient failures with bounded exponential backoff, and
exposes status. Workers are replaceable processes; a restart must not erase
adapter identity or durable state. Adapters normally run as supervised worker
processes outside the harness because they are long-lived, protocol-specific,
failure-prone, and dependency-heavy.

### State and ownership

Durable adapter state includes identity and configuration, secret references,
connection material, provider cursors and resume tokens, target-to-conversation
routing, inbound deduplication keys, the inbound event record, the outbound
queue with delivery attempts and acknowledgements, and health/last-error
information. Workers may cache, but durable state must survive worker and host
restarts.

Ownership:

- The **host runtime** owns adapter identity, lifecycle, routing, durable
  inbox/outbox records, retries, and conversation wakeups.
- The **adapter implementation** owns protocol-specific connection logic,
  normalization, acknowledgements, and provider-specific durable data.
- The **conversation** owns the resulting turns and decisions.

### Inbound events

1. Receive and authenticate the external event.
2. Normalize into a generic envelope.
3. Deduplicate on a stable provider event/message id.
4. Store full payloads and large bodies as adapter data or artifacts.
5. Append a compact durable event record.
6. Route to the correct agent and conversation.
7. Queue a conversation wakeup.
8. Acknowledge the provider at the protocol-appropriate point.

Assume at-least-once delivery: deduplication and idempotency are requirements,
not optimizations. Wakeups carry only enough to understand the event and
locate its payload; large content stays artifact-backed.

### Outbound messages

Sending is always an explicit tool call, never implicit model output:

```text
send_adapter_message(...)
    -> validate authorization and destination
    -> append durable outbox record
    -> return queued acknowledgement

adapter worker
    -> read outbox
    -> send through external service
    -> record provider acknowledgement or failure
    -> retry according to policy
```

This keeps turns from blocking on unreliable external services and makes side
effects inspectable and recoverable. Results distinguish **queued** from
**delivered**; a successful enqueue is never reported as confirmed delivery.

### Command protocol and adapter tools

Every adapter is managed by the same generic lifecycle tools (`create_adapter`
etc. above). Adapters do not implement their own `start_discord`-style
lifecycle tools; the host supervisor owns lifecycle semantics.

Beyond lifecycle, the worker protocol supports a generic command envelope:

```json
{
  "commandId": "uuid",
  "adapterId": "uuid",
  "command": "send_message",
  "arguments": { "target": "channel-123", "text": "hello" }
}
```

with responses `accepted`, `completed` + result, or `failed` + error. The host
owns command ids, durable queueing, authorization, and timeouts; the worker
owns protocol-specific validation and execution. Commands declare completion
semantics: **immediate query** (bounded result during the call), **queued
operation** (accepted + command id; delivery completes later), or
**long-running operation** (progress arrives as later adapter events).

Adapter manifests declare typed commands (name, schema, completion mode,
side-effect classification), and the harness generates model-facing tools from
those declarations — `schedule_task`, `search_email`, `add_reaction` — routing
each through the internal `invoke_adapter(adapter_id, command, arguments)`
boundary. Generated typed tools beat one untyped `invoke_adapter` tool for
discoverability and validation. Name collisions resolve deterministically via
qualified names (`scheduler.schedule_task`) or explicit aliases.

Three patterns follow:

1. **Inbound-only adapter** — no extra tools; it emits events and wakes the
   agent.
2. **Common messaging adapter** — uses the shared `send_adapter_message`.
3. **Domain-specific adapter** — declares typed commands in its manifest.

### Provenance

Adapters use the same provenance categories as tools, and every adapter
resolves to the same versioned worker protocol under the same supervisor.

**There are no built-in adapters.** The core adapter runtime — supervision,
durable inbox/outbox, routing, wakeups, health — is a host capability, not an
adapter. `exo repl` is a direct, trusted Exoharness client, not an adapter,
so an operator can always reach a conversation even when the adapter
supervisor or every worker is unhealthy. A minimal installation has adapter
capability with zero adapters installed.

Agent-authored adapters are generated workers, validated and launched through
the normal supervisor with restricted capabilities and agent-local trust
scope. They must implement the full worker protocol (health, message ids,
acknowledgements, graceful shutdown) and are disabled automatically after
repeated permanent failures. Publishing one is a separate, reviewed, signed
release. Third-party adapters need stricter manifest review than tools because
they run continuously and accept untrusted external input: manifests must
disclose listeners, outbound domains, secrets, filesystem access, event types,
side effects, and retry/deduplication behavior.

## Core Adapters

The practical profile installs and enables exactly two Exo-maintained
adapters. All others are opt-in.

### Task scheduler

Durable autonomous work needs both ways to start a turn: adapters wake Exo
when the outside world has something new; the scheduler wakes Exo when Exo
decided to continue later.

The scheduler is a **system adapter with command tools** — not part of
Exoharness (calendar/cron semantics are replaceable policy) and not merely a
tool (something must stay alive between calls):

```text
schedule_task tool
    -> generic invoke_adapter command
    -> scheduler adapter stores schedule
    -> tool returns schedule id

clock reaches due time
    -> scheduler adapter emits timer_fired event
    -> Exoharness conversation-event ingress
    -> agent wakeup turn
```

Its manifest declares `schedule_task`, `list_scheduled_tasks`, and
`cancel_scheduled_task`. Exoharness only authenticates and delivers the
resulting wakeup event.

### ExoChat

ExoChat (`exochat`) gives every practical installation a credential-free
browser chat surface: no bot tokens, no device linking. Inbound messages wake
the agent; outbound replies use the generic `send_adapter_message`.

ExoChat depends on a hosted relay. Failure to reach it must not block Exo
startup, scheduler operation, or `exo repl` access — the REPL remains the
direct local fallback.

## Other Standard-Library Adapters

Standard-library adapters are optional protocol integrations maintained and
signed by the core Exo team. They receive no privileged protocol path: same
worker protocol, state model, supervision, and capability declarations as any
third-party adapter. Their distinction is publisher and support level.

- **Discord** — bot-token integration with message, attachment, and optional
  voice support. Declares typed commands (e.g. `add_reaction`,
  `create_thread`) beyond generic send. Configurable wake triggers
  (`mentions_only`, `all_messages`) and channel allowlists.
- **Slack** — bot-token integration; same wake-trigger and routing model.
- **IRC** — channel-based chat with optional server password via secret
  reference.
- **Signal** — linked-device integration (`signal-cli`); contact allowlists.
- **WhatsApp** — linked-device integration; contact and chat allowlists.
- **Email** — illustrates the tool/adapter split: receipt is adapter work
  (webhook verification, deduplication, wakeups); replying is a typed
  `send_email` command tool; inbox search is a command tool over the
  adapter's durable store.
- **`agent-cli`** — a local unix-socket transport so shell invocations can
  prompt the agent from any directory. It is an adapter (it listens
  continuously and wakes an idle agent), unlike `exo repl` (a direct client).

Credentials are always secret references in adapter configuration, resolved by
the host at worker launch; configs never contain raw tokens.

## Design Considerations

### Boundaries

- **Turn boundary** — tool execution belongs inside a turn; adapter workers
  must never depend on a turn staying open.
- **Storage boundary** — conversation history records compact tool
  requests/results, wakeups, and outbound intentions. Full payloads live in
  artifacts or subsystem stores. Adapter state is independent of model
  context: compaction or conversation restart must not erase cursors,
  deduplication records, or pending sends.
- **Security boundary** — secrets are referenced by id and resolved only at
  execution; definitions and prompts never contain credentials; external
  input is untrusted, including text that becomes wakeup prompts; outbound
  side effects require explicit agent action and policy checks. Adapters add
  inbound-surface controls: sender allowlists, webhook authentication, rate
  limiting, payload size limits, and wakeup throttling.
- **Concurrency boundary** — adapters and targets may process concurrently,
  but turns for one conversation are serialized. The adapter runtime
  implements backpressure: bursts must not create unbounded turns; coalescing,
  batching, per-conversation queues, and rate limits are explicit policies.

### Failure semantics

A tool failure is scoped to one invocation: validate before side effects,
return a structured error, record whether a partial side effect may have
occurred, and let the model decide next steps.

An adapter failure is a lifecycle event: record health and last error,
preserve cursors and the outbound queue, restart with bounded backoff and
jitter, distinguish permanent configuration errors from transient failures,
avoid waking the agent repeatedly for one failure, and surface exhausted
retries to the operator.

### Tool registry

Tools are distributed through a registry with several layers:

1. **Remote catalog and artifact store** — external infrastructure holding
   signed manifests and immutable, content-addressed release artifacts.
   Reading metadata never runs tool code.
2. **Local registry installer** — a host-owned component (also exposed as the
   `exo registry` CLI) that searches catalogs, verifies signatures and
   digests, applies policy, maintains the local store and lockfile, and
   performs atomic installation.
3. **Runtime tool registries** — per-agent/per-conversation resolved
   definitions and handlers, containing only tools explicitly installed for
   that scope.

The model-facing surface is only `inspect_tools(source = "registry", ...)` for
discovery and `manage_tool` for lifecycle. Entries have stable names
(`tool:exo/web-search`, `tool:local/<agent-id>/log-analyzer`); the `exo`
publisher namespace is reserved for the standard library, and signatures — not
names — establish trust. Manifests carry schemas, capabilities, secret names,
compatibility ranges, digests, and signatures. The lockfile records name,
exact version, digest, publisher, granted capabilities, and configuration
reference, making installations reproducible. Updates produce a capability
diff; a new version never silently gains permissions. Reuse an existing
artifact format (e.g. OCI) for distribution; the Exo-specific work is the
manifest, capability model, and runtime activation.

Adapter registry distribution follows the same principles but is a separate
design decision.

### Anti-patterns

- **A tool with a hidden background loop** — an untracked listener that later
  mutates conversation state is an adapter without the lifecycle, storage,
  and supervision model.
- **An adapter whose state exists only in memory** — if a restart loses
  routing, cursors, deduplication, or pending sends, the architecture is
  incomplete.
- **Implicit replies** — receiving a message must not auto-send the model's
  final text back. The wakeup creates a normal turn; an explicit outbound
  tool call expresses the decision to cause a side effect.
- **Holding a turn open for future events** — register an adapter or durable
  subscription, finish the turn, and let the event wake the agent.
- **Treating enqueue as delivery** — queued and provider-acknowledged are
  distinct states everywhere they surface.
- **Full external payloads in prompts** — external content can be large,
  sensitive, or adversarial; wakeups use compact envelopes plus artifact
  references.

### Minimality principle

The goal is to minimize permanently trusted **capabilities**, not the count of
model-facing names. Folding unrelated operations into one large `manage_exo`
tool would shrink the tool list without shrinking the attack surface.
Everything outside the four built-in tools — sandbox management, adapter
control, code mode, the two core adapters — is standard-library, installed by
profile, and replaceable without changing the trusted core.
