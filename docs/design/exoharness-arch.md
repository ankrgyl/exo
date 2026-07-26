# Exoharness Architecture

This document describes the full Exoharness API from the perspective of the
subcomponents that use it. It covers the core Rust object model, the JSONL
protocol used to expose that model across process boundaries, the TypeScript
harness API, and the way higher-level systems such as the executor, scheduler,
adapters, CLI, and Exo compose those pieces.

## Mental Model

Exoharness is the durable runtime substrate for agents. It owns agents,
conversations, sessions, turns, events, artifacts, sandboxes, bindings, and
secrets. The executor layer sits on top and decides how a turn is run: a basic
LLM loop, an RLM loop, or a TypeScript harness. Product-specific systems such as
Exo then add tools, prompts, schedulers, and adapters on top of that
executor layer.

The main boundary looks like this:

```text
CLI / Exo scripts / scheduler / adapter runner
  -> executor Harness facade
    -> exoharness object model
    -> executor turn runtime
      -> TypeScript runner, basic harness, or RLM harness
        -> tools, model calls, sandbox process APIs
```

The core API is intentionally small. Most operations are scoped to one of four
handle types:

- `ExoHarness`: root handle for global agents, bindings, and secrets.
- `AgentHandle`: per-agent timeline, conversations, artifacts, bindings, and
  secrets.
- `ConversationHandle`: per-conversation event log, artifacts, sandboxes,
  bindings, and secrets.
- `TurnHandle`: active-turn-only writes that must preserve turn consistency.

The distinction between `ConversationHandle` and `TurnHandle` is important.
Conversation-level writes append to the conversation head immediately. Turn-level
writes attach the active turn's session and turn ids to appended messages and
artifacts. Code that runs inside a turn should prefer `TurnHandle` for messages
and artifacts so the event log preserves turn ownership.

## Key Crates And Files

- `crates/exoharness/src/types.rs`: core Rust traits and data types.
- `crates/exoharness/src/basic.rs`: filesystem-backed implementation of the
  core traits.
- `crates/exoharness/src/protocol.rs`: JSON-serializable request and response
  protocol for the core API.
- `crates/exoharness/src/server.rs`: protocol server that dispatches JSON
  requests into an `ExoHarness`.
- `crates/executor/src/harness_types.rs`: higher-level executor-facing harness
  facade.
- `crates/executor/src/harness_executor.rs`: generic turn execution lifecycle.
- `crates/executor/src/typescript.rs`: Rust host for TypeScript harness
  processes.
- `typescript/harness/index.ts`: public TypeScript API exposed to harness
  authors.
- `typescript/harness/runner.ts`: TypeScript guest process that converts the
  public TypeScript API into host protocol messages.
- `examples/exo/harness.ts`: Exo's TypeScript harness module.

## Core Rust API

The core Rust API lives in the `exoharness` crate. Callers generally work with
trait objects (`Arc<dyn ExoHarness>`, `Arc<dyn AgentHandle>`,
`Arc<dyn ConversationHandle>`, and `Arc<dyn TurnHandle>`) rather than concrete
storage implementations.

### Root: `ExoHarness`

The root handle manages global state:

- `list_agents() -> Vec<AgentHandle>`
- `get_agent(id) -> Option<AgentHandle>`
- `new_agent(NewAgentRequest) -> AgentHandle`
- `delete_agent(id) -> bool`
- `list_bindings() -> Vec<BindingMetadata>`
- `put_binding(Binding) -> BindingId`
- `get_binding(id) -> Option<Binding>`
- `list_secrets() -> Vec<SecretMetadata>`
- `put_secret(PutSecretRequest) -> SecretId`
- `get_secret(id) -> Option<Secret>`

Global bindings and secrets act as defaults. Agent and conversation scopes can
override them by name.

### Agent: `AgentHandle`

An agent handle exposes conversations and agent-scoped resources:

- `record() -> AgentRecord`
- `get_events(AgentEventQuery) -> GetAgentEventsResult`
- `watch_events(after_exclusive) -> AgentEventStream`
- `get_event(agent_event_id) -> Option<AgentEvent>`
- `add_events(AddAgentEventsRequest) -> AddAgentEventsResult`
- `ensure_execution_epoch(EnsureExecutionEpochRequest) -> EnsureExecutionEpochResult`
- `list_conversations() -> Vec<ConversationHandle>`
- `get_conversation(id) -> Option<ConversationHandle>`
- `new_conversation(NewConversationRequest) -> ConversationHandle`
- `delete_conversation(id) -> bool`
- `list_bindings()`, `put_binding()`, `get_binding()`
- `list_secrets()`, `put_secret()`, `get_secret()`
- `list_artifacts()`, `write_artifact()`, `read_artifact()`

Agent-scoped artifacts are useful for configuration and data that should live
with the agent instead of one conversation. Mutations append typed events to
the agent timeline. Secret events contain metadata, never secret values.
Epoch requests may include the agent head observed before manifest assembly;
the backend rejects the request if that head changed before epoch selection.

### Conversation: `ConversationHandle`

A conversation handle owns the event log and the conversation-scoped execution
environment:

- `record() -> ConversationRecord`
- `start_session() -> SessionId`
- `end_session(session_id)`
- `begin_turn(BeginTurnRequest) -> TurnHandle`
- `get_events(EventQuery) -> GetEventsResult`
- `watch_events(after_exclusive) -> EventStream`
- `get_event(event_id) -> Option<Event>`
- `add_events(AddEventsRequest) -> AddEventsResult`
- `fork(ForkConversationRequest) -> ConversationHandle`
- `list_artifacts()`, `write_artifact()`, `read_artifact()`
- `create_sandbox(CreateSandboxRequest) -> SandboxId`
- `snapshot_sandbox(sandbox_id) -> SnapshotId`
- `start_sandbox(StartSandboxRequest)`
- `stop_sandbox(sandbox_id)`
- `run_in_sandbox(RunInSandboxRequest) -> SandboxProcess`
- `list_bindings()`, `put_binding()`, `get_binding()`
- `list_secrets()`, `put_secret()`, `get_secret()`

Conversation-level `add_events()` is append-only. Callers that need to avoid
overlapping agent runs should serialize through the executor-level `send()` APIs
or another higher-level owner/lease.

### Turn: `TurnHandle`

A turn handle is the active-turn write surface:

- `record() -> TurnRecord`
- `add_events(Vec<EventData>) -> AddEventsResult`
- `write_artifact(WriteArtifactRequest) -> ArtifactVersion`
- `finish() -> EventId`

Turn writes append events tagged with the turn's session and turn ids.
`finish()` writes `turn_ended` and marks the handle complete. Calling
`finish()` again is idempotent and returns the existing finish event id.

## Records And IDs

Most persisted records use UUIDv7 IDs. These IDs carry sortable timestamps,
which makes event ordering straightforward.

Important records:

- `AgentRecord`: `id`, `slug`, `name`, `latest_event_id`,
  `active_execution_epoch_id`.
- `ConversationRecord`: `id`, `slug`, `name`, `latest_event_id`.
- `TurnRecord`: `id`, `session_id`, `agent_event_id`, `execution_epoch_id`.
- `AgentEvent`: `id`, `agent_id`, optional origin, `created_at`, and tagged
  `data`.
- `ExecutionEpochRecord`: `id`, manifest SHA-256 digest, immutable manifest,
  and `created_at`.
- `Event`: `id`, `conversation_id`, optional `session_id`, optional `turn_id`,
  `created_at`, and tagged `data`.
- `ArtifactVersion`: `artifact_id`, `path`, `version`, `created_at`,
  `size_bytes`.

The public TypeScript API translates Rust snake_case fields into camelCase,
while the wire protocols preserve serde's snake_case tags and fields.

## Event Log API

The agent and conversation event logs are the durable source of truth for
shared state, conversation history, and runtime side effects. Events are
append-only.

The agent timeline records agent/conversation lifecycle, agent artifacts,
bindings, secret metadata, agent sandbox events, execution epoch creation and activation,
and custom events. An optional origin links an agent event back to the
conversation, session, and turn that caused it.

Core event variants:

- `conversation_forked`
- `session_started`
- `session_ended`
- `turn_started`
- `turn_ended`
- `messages`
- `tool_requested`
- `tool_result`
- `artifact_written`
- `sandbox_created`
- `sandbox_started`
- `sandbox_stopped`
- `sandbox_snapshotted`
- `custom`

`messages` events contain Lingua messages. `tool_requested` and `tool_result`
events preserve tool execution state across turns. `artifact_written` events
reference artifact metadata; artifact bytes live in artifact storage. `custom`
events are available for subcomponents that need typed event payloads without a
new core event variant.

`EventQuery` supports:

- `cursor`: event id boundary.
- `direction`: `asc` or `desc`.
- `limit`: max number of events.
- `session_id`: filter to one session.
- `turn_id`: filter to one turn.
- `types`: filter by event type string.

`watch_events()` returns existing events after the requested bound and then live
events through an in-memory subscriber list. The basic implementation is useful
inside one process; it is not a cross-process pub/sub service.

## Sessions And Turns

A session groups related turns. `begin_turn()` accepts an optional `session_id`.
If none is provided, Exoharness creates a new session and appends
`session_started` before `turn_started`. If input messages are included, they
are appended as a `messages` event in the same turn.

A turn's causal coordinates are its conversation position, `agent_event_id`,
and `execution_epoch_id`. `begin_turn()` accepts optional explicit agent and
epoch pins and rejects an epoch activated after the pinned agent head. When
callers omit both, the basic backend captures the current agent head and active
epoch while beginning the turn.

Before `send()` or `send_stream()` begins a turn, the executor ensures a
content-addressed execution epoch. Its manifest contains effective agent and
conversation config, visible bindings, visible secret metadata, executor
crate identity, and SHA-256 hashes for configured TypeScript harness/tool
modules. Matching manifests reuse or reactivate an existing immutable epoch;
previously unseen manifests append `execution_epoch_created` with the manifest.
Reactivating a known epoch appends the compact, id-only
`execution_epoch_activated`.

The executor-level `send()` API starts a turn, executes the configured harness,
and finishes the turn. External systems such as adapters and scheduler wakeups
usually call `send()` with a fresh session and then close the session after the
wakeup completes.

For code running during a turn, use `context.exoharness.current.turn` instead of
`context.exoharness.current.conversation` when appending messages or artifacts.
For code outside a turn, serialize wakeups per conversation if multiple external
sources can fire concurrently.

## Artifacts

Artifacts are versioned blobs addressed by logical path. Writing to the same
path creates a new version of the same artifact id. Writing to a new path
creates a new artifact id.

Artifact APIs exist at agent and conversation scope. Conversation-level
`write_artifact()` appends an `artifact_written` event to the conversation.
Turn-level `write_artifact()` also appends an `artifact_written` event, but it
tags the event with the active turn's session and turn ids.

TypeScript convenience methods:

- `writeArtifact({ path, contents })`
- `writeArtifactText({ path, text })`
- `writeArtifactJson({ path, value })`
- `readArtifact({ artifactId, version })`
- `readArtifactText(...)`
- `readArtifactJson<T>(...)`

The TypeScript tool registry writes full tool results to artifacts by default.
Large tool results are compacted in the conversation history while full
stdout/stderr/result JSON remains available under `tool-results/...`.

## Sandboxes

Sandboxes are conversation-scoped in the core API. A sandbox record stores:

- image
- default workdir
- filesystem mounts
- networking flag
- idle timeout
- running state
- latest snapshot id

The core methods create, start, stop, snapshot, and run processes in a sandbox.
`run_in_sandbox()` returns a `SandboxProcess` with async stdout, stderr, stdin,
and wait handles.

The executor adds policy on top:

- `ensure_conversation_sandbox()` creates or reuses the conversation sandbox.
- `ensure_agent_sandbox()` lets Exo share one persistent sandbox across an
  agent by using agent-level sandbox metadata.
- The scheduler supports `agent`, `conversation`, and `task_fresh` modes.

From TypeScript, `context.startSandboxProcess({ command, env })` starts a
sandbox process and returns a `SandboxProcess` with `ReadableStream<string>`
stdout/stderr, `writeStdin()`, `closeStdin()`, `close()`, and `wait()`.

## Bindings And Secrets

Bindings and secrets exist at root, agent, and conversation scope.

Bindings:

- `env`: name, environment variable name, secret id.
- `mcp`: name, server URL, optional secret id.
- `llm`: name, model, optional base URL, optional secret id.

Secrets:

- `key`: raw API key or token.
- `oauth`: access token and optional refresh token.

The basic implementation encrypts secrets at rest through a configured secret
backend. Current backends include Apple Keychain, file-backed master key, and a
static key option for tests.

When listing bindings or secrets from agent or conversation scope, metadata is
merged by name. More local scopes override broader scopes.

## Core JSONL Protocol

The `exoharness::protocol` module exposes the core Rust API as a JSONL request
protocol. It is used by the TypeScript runner when TypeScript code calls
`context.exoharness`.

Every client message is:

```json
{ "kind": "request", "id": 1, "request": { "type": "list_agents" } }
```

Every server message is:

```json
{
  "kind": "response",
  "id": 1,
  "ok": true,
  "response": { "type": "agents", "agents": [] },
  "error": null
}
```

The protocol request variants mirror the core handles:

- Root: `list_agents`, `get_agent`, `new_agent`, `delete_agent`,
  `list_bindings`, `put_binding`, `get_binding`, `list_secrets`, `put_secret`,
  `get_secret`.
- Agent: `list_conversations`, `get_conversation`, `new_conversation`,
  `delete_conversation`, `agent_list_artifacts`, `agent_read_artifact`,
  `agent_write_artifact`, agent bindings, agent secrets.
- Conversation: `conversation_start_session`, `conversation_end_session`,
  `conversation_get_events`, `conversation_get_event`,
  `conversation_add_events`, `conversation_fork`,
  `conversation_list_artifacts`, `conversation_read_artifact`,
  `conversation_write_artifact`, conversation bindings, conversation secrets.
- Turn: `turn_add_events`, `turn_write_artifact`, `turn_finish`.

The protocol addresses turns by durable ids. TypeScript code receives the active
turn's agent, conversation, session, and turn ids, and subsequent turn requests
send those ids back to the host.

## Executor Harness Facade

The executor crate wraps the core API with a product-facing `Harness` facade:

- `Harness`: list/create/delete agents, resolve agents by id or slug, flush
  tracing, and expose the underlying `ExoHarness`.
- `HarnessAgent`: read/write agent config, list/create/delete conversations,
  and expose the underlying `AgentHandle`.
- `HarnessConversation`: read/write conversation config, read/write model
  overrides, materialize messages, close sessions, send turns, stream turns, and
  expose the underlying `ConversationHandle`.

This facade adds configuration artifacts and model execution semantics to the
raw storage API. Callers that want to run the agent should use
`HarnessConversation::send()` or `send_stream()`. Callers that only need storage,
artifacts, events, or sandbox execution can use `exoharness_handle()`.

## Turn Execution Lifecycle

`ExecutorHarnessRuntime` implements the shared send lifecycle:

1. Load agent config.
2. Load conversation config.
3. Load any conversation-level model override.
4. Let the selected executor prepare the conversation.
5. Prepare the request.
6. Call `conversation.begin_turn()`.
7. Execute the turn with streaming enabled or disabled.
8. Append `turn_ended` through the turn handle.

The actual turn runner is a `HarnessExecutor` implementation. Current executor
implementations include the basic harness, RLM harness, and TypeScript harness.

Streaming turns return an `ExecutionStreamHandle` and emit:

- first-chunk timing
- text deltas
- tool calls
- tool results
- final completion or error

## TypeScript Harness Host Protocol

The TypeScript executor runs one persistent Node process per harness module
path. The host command is:

```text
node --import tsx typescript/harness/runner.ts <modulePath>
```

Rust writes host-to-guest JSONL messages to stdin. TypeScript writes
guest-to-host JSONL messages to stdout. Stderr is captured for error reporting.
Runner processes are cached by module path and removed from the cache if a turn
fails.

Host-to-guest messages:

- `init`: starts a turn and includes agent, conversation, turn, configs, request,
  streaming flag, and optional tracing parent.
- `shutdown`: closes the runner loop.
- `runtime_response`: response to a runtime request.
- `exo_response`: response to a core Exoharness API request.
- `runtime_event`: async sandbox process output, exit, or error.

Guest-to-host messages:

- `runtime_request`: asks Rust to execute a tool or manage a sandbox process.
- `exo_request`: asks Rust to perform a core Exoharness protocol request.
- `stream_event`: forwards stream output to the executor stream.
- `done`: marks the TypeScript turn complete.
- `error`: fails the turn with message and stack.

The TypeScript runner imports the harness module once, then loops over `init`
messages. A module must export either a default harness or a named `harness`
export with `runTurn(context)`.

## TypeScript Public API

The public TypeScript API is exported from `@exo/harness`.

A harness module is:

```ts
import { defineHarness } from "@exo/harness";

export default defineHarness({
  async runTurn(context) {
    // Run one turn.
  },
});
```

`TurnContext` exposes:

- `agentConfig`: model, instructions, TypeScript module config, tool settings,
  sandbox image, networking, token and tool round-trip limits, and tracing
  config.
- `conversationConfig`: networking, shell program, sandbox scope, and mounts.
- `request`: input messages and optional session id.
- `streaming`: whether stream events should be emitted.
- `braintrustParent`: optional tracing parent id.
- `exoharness`: current and global Exoharness API.
- `executeTool(request)`: ask the Rust tool runtime to execute a configured
  tool.
- `startSandboxProcess(request)`: start an interactive sandbox process.
- `executePendingTools(toolCalls)`: convenience for sequential tool execution.
- `stream`: first chunk, text, tool-call, and tool-result stream emitters.

`context.exoharness.current` provides the current:

- `agent`
- `conversation`
- `turn`

The `Agent`, `Conversation`, and `Turn` TypeScript objects intentionally expose
the subset of the Rust handles needed by harness code, with camelCase fields and
convenience methods for text and JSON artifacts. Bindings and secrets are
readable from TypeScript, but binding/secret mutation remains on the Rust core
protocol and executor configuration paths rather than the public TypeScript
harness API.

Helpers in `@exo/harness` include:

- `messagesEvent()`, `toolRequestedEvent()`, `toolResultEvent()`
- `appendMessages()`, `appendCustomEvent()`, `replyText()`
- `getMessages()`, `materializeConversationMessages()`
- `materializePromptMessages()`
- `messagesToHistoryMessages()`, `messagesToTranscript()`
- `projectAnthropicMessageToolEvents()`
- `assertRoundBudget()`
- `turnMetadata()`

## Tool API

Tools are represented as `ToolInstance` values:

- `definition`: name, description, JSON-schema parameters, optional output
  schema.
- `source`: `built_in`, `library`, or `agent`.
- `handler.execute(args, execution)`: implementation.

Reusable tools can be authored with `defineTool()`. Tool modules can be loaded
from library paths or from agent-created tool directories.

Tool definitions are validated:

- `name` must be non-empty, at most 64 characters, and only contain letters,
  numbers, underscores, and dashes.
- `parameters` must be an object JSON schema.
- `parameters.additionalProperties` must be `false`.
- Handlers must implement `execute`, not `invoke`.

Tool execution flow:

1. The harness registers tools in a `HarnessToolRegistry`.
2. The model requests one or more tool calls.
3. The harness appends `tool_requested` events through the active turn.
4. `executePendingTools()` or the registry executes tools.
5. Tool results are normalized, streamed if needed, compacted into artifacts,
   and appended as `tool_result` events.

Large tool results are not kept fully inline in the message history. The compact
result contains preview text, truncation metadata, and artifact references.

## Model Loop Integration

The TypeScript harness API does not directly prescribe a model loop. Exo and
the basic TypeScript harness use shared turn-loop helpers to:

1. Materialize prompt messages from configured instructions and conversation
   events.
2. Register built-in, library, adapter, scheduler, and agent-created tools.
3. Call the model.
4. Project model text and tool calls into Exoharness events.
5. Execute tools and continue until the model finishes or the configured tool
   round-trip budget is reached.

The Rust model runtime uses Lingua's universal message and tool abstractions and
routes requests through `braintrust_llm_router`.

## Exo Integration

`examples/exo/harness.ts` is a TypeScript harness module. It composes the
generic TypeScript harness API with Exo-specific instructions and tools.

On each turn it:

1. Calls `runResponsesHarnessTurn()`.
2. Adds generic basic harness instructions.
3. Adds `examples/exo/prompts/me.md`.
4. Adds an optional local profile prompt from `.exo/exo-profile.md` or
   `EXO_LOCAL_PROMPT_FILE`.
5. Registers built-in tools.
6. Registers scheduler tools.
7. Registers adapter tools.
8. Registers configured library tool modules.
9. Registers agent-created tools if enabled.

Exo-specific tools are still executed by the Rust tool runtime. The
TypeScript side mostly provides model-visible schemas and forwards scoped
arguments such as current agent id and conversation id.

## Scheduler Integration

The scheduler is not part of the core Exoharness crate. It is an executor-level
service that uses Exoharness primitives.

The scheduler tool API is model-facing TypeScript:

- `schedule_sandbox_task`
- `list_scheduled_tasks`
- `cancel_scheduled_task`
- `delete_scheduled_task`

Those tool handlers call `context.executeTool()` with current agent and
conversation ids. The Rust `ExoToolRuntime` persists scheduled task records
outside the core conversation event log.

When a task is due, the scheduler runtime:

1. Resolves the agent through the `Harness` facade.
2. Resolves the conversation through the `HarnessAgent`.
3. Loads agent and conversation config.
4. Resolves a sandbox according to task mode.
5. Runs setup and command processes through `ConversationHandle::run_in_sandbox`.
6. Writes a run artifact through `ConversationHandle::write_artifact`.
7. Wakes the conversation with a prompt describing the result.

The wakeup uses `HarnessConversation::send()`, so the task result becomes a
normal agent turn. A per-conversation wakeup lock serializes these external
wakeups so two scheduler/adapter triggers do not run the same conversation at
once.

## Adapter Integration

Adapters are also executor-level services, not core Exoharness concepts. They
bridge external networks such as IRC, WhatsApp, and Signal into conversations.

Adapter runtime flow:

1. The adapter runner loads enabled adapter records from `AdapterStore`.
2. It resolves the configured agent and conversation through the `Harness`
   facade.
3. It starts the adapter worker process.
4. Worker inbound messages are recorded in the adapter store.
5. Matching inbound messages wake the conversation with a user prompt.
6. The agent can intentionally respond by calling `send_adapter_message`.
7. Outbound messages are queued in `AdapterStore`.
8. The worker loop drains outbound messages and sends them externally.

Inbound and outbound adapter state is intentionally stored in `AdapterStore`
rather than as conversation artifacts. Adapter events can arrive while an agent
turn is running, and storing them outside the conversation log avoids unrelated
history churn.

Like the scheduler, adapter wakeups use the per-conversation wakeup lock before
calling `HarnessConversation::send()`.

## CLI Integration

The CLI mostly talks to the executor `Harness` facade. It uses the facade to
create agents, configure agent harness kind, create conversations, send turns,
stream turns, inspect events/messages, and manage bindings/secrets. It should
not need to know whether the underlying storage is `BasicExoHarness` or another
future implementation.

Exo-specific startup logic lives under `examples/exo`. The root CLI
still provides generic agent and conversation operations; Exo scripts and
runner binaries compose those generic operations with Exo scheduler and
adapter services.

## Storage Implementation

`BasicExoHarness` is the current local implementation. It stores records and
artifacts under a filesystem-backed object store rooted at the configured
harness root.

Important properties:

- A single async write lock serializes writes inside one process.
- Event files are sorted by UUIDv7 event id.
- Conversation records cache `latest_event_id`.
- Subscribers are in-memory and per-process.
- Secrets are encrypted before writing.
- Artifact metadata and bytes are written separately.
- Sandboxes are tracked in metadata plus an in-memory running-sandbox table.

Because some coordination is in-memory, multiple independent processes writing
to the same harness root should be treated carefully. The basic backend is
primarily designed for a coordinated local runtime.

## Concurrency Rules

Use these rules when adding new subcomponents:

- If code is running inside a turn, append messages and artifacts through
  `TurnHandle` or `context.exoharness.current.turn`.
- If code is outside a turn and wants the agent to react, call the executor
  `send()` API rather than appending assistant messages directly.
- If multiple external sources can wake the same conversation, serialize those
  wakeups.
- Do not write conversation artifacts from adapter or scheduler plumbing while a
  wakeup turn is active.
- Keep durable subsystem queues in their subsystem stores when the data is not
  part of the agent-visible conversation history.

## Extending Exoharness

Add to the core `exoharness` API only for durable, generic agent runtime
concepts. Good candidates are things every harness implementation should support
or expose consistently, such as conversations, events, artifacts, and sandboxes.

Prefer executor- or product-level extensions when the feature is specific to a
runtime or application:

- Scheduler tasks live in executor/Exo code because they are a particular
  use of sandboxes and wakeups.
- Adapters live in executor/Exo code because IRC, WhatsApp, and Signal are
  external integration services.
- Tool definitions live in TypeScript and executor tool runtimes because they
  are model-facing behavior, not storage primitives.

When adding a new core operation, update all three surfaces:

1. Rust traits and implementation.
2. `exoharness::protocol` request/response variants and server dispatch.
3. TypeScript raw protocol types plus public wrapper methods.

When adding a TypeScript-only runtime capability, update both sides of the
TypeScript harness protocol:

1. `crates/executor/src/typescript.rs` host message/request/response handling.
2. `typescript/harness/runner.ts` raw types and `TurnContext` implementation.
3. `typescript/harness/index.ts` public type definitions.

## Common Call Flows

### User Sends A CLI Message

```text
CLI
  -> HarnessConversation::send()
    -> ExecutorHarnessRuntime::send()
      -> ConversationHandle::begin_turn()
      -> selected executor runs turn
      -> TurnHandle::add_events(...)
      -> TurnHandle::finish()
```

### TypeScript Harness Calls A Core API

```text
harness module
  -> context.exoharness.current.conversation.getEvents(...)
    -> runner.ts sends { kind: "exo_request" }
      -> TypeScriptRunnerProcess receives request
        -> ExoHarnessServer::handle_request()
          -> ConversationHandle::get_events()
        -> host sends { kind: "exo_response" }
      -> runner resolves TypeScript promise
```

### TypeScript Harness Executes A Tool

```text
harness module
  -> context.executeTool({ functionName, arguments })
    -> runner.ts sends { kind: "runtime_request", type: "execute_tool" }
      -> TypeScriptRunnerProcess calls ToolRuntime::execute()
      -> host sends runtime_response tool_result
    -> harness appends tool_result event through TurnHandle
```

### TypeScript Harness Starts A Sandbox Process

```text
harness module
  -> context.startSandboxProcess({ command, env })
    -> runner.ts sends runtime_request start_sandbox_process
      -> Rust ensures conversation sandbox
      -> ConversationHandle::run_in_sandbox()
      -> host returns process_id
      -> host streams runtime_event sandbox_process_output/exit/error
    -> TypeScript SandboxProcess exposes streams and wait()
```

### Scheduled Task Wakes A Conversation

```text
scheduler runner
  -> run_due_tasks()
    -> resolve HarnessAgent and HarnessConversation
    -> run command in sandbox through ConversationHandle
    -> write result artifact through ConversationHandle
    -> send_conversation_wakeup()
      -> per-conversation lock
      -> HarnessConversation::send()
      -> HarnessConversation::close_session()
```

### Adapter Message Wakes A Conversation

```text
adapter worker
  -> WorkerEvent::Message
    -> AdapterStore records inbound event
    -> send_conversation_wakeup()
      -> per-conversation lock
      -> HarnessConversation::send()
      -> HarnessConversation::close_session()
```

## Current Caveats

- The basic backend's event subscribers and running sandbox table are
  in-memory, so they are not a distributed coordination layer.
- TypeScript turn requests carry durable turn ids, but there is no separate
  turn lease token. Coordinate concurrent writers at the executor/wakeup layer.
- Tool results are compacted for model history, so callers should follow
  artifact references for full output.
- TypeScript runner protocol messages are line-delimited JSON. Anything written
  to stdout by the runner process must be protocol JSON; diagnostics should go
  to stderr.
- Scheduler and adapters are users of Exoharness, not core Exoharness features.
  Their durable stores are separate from the core event log.
