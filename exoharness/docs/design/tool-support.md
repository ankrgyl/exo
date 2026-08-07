# Tool Support Plan

## Context

`exo` separates the trusted exoharness substrate from executor-owned agent
semantics. Tool support should follow the same split:

- The exoharness owns durable state, events, artifacts, bindings, secrets, and
  sandbox execution.
- Executors and harness modules own which model-facing tools exist, how they
  are exposed, how calls are authorized, and how calls are dispatched.

The tool system should make it easy for harnesses to expose a small set of
model-callable functions without turning exoharness into a product-specific
integration registry. The exoharness can already store bindings and secrets.
Tools should use those generic substrate capabilities when they need
configuration or credentials, but tool semantics remain above the substrate.

## Goals

- Let TypeScript harnesses compose tools without hard-coding every tool in Rust.
- Keep the model-facing tool contract portable across model runtimes.
- Preserve `tool_requested` and `tool_result` events as the canonical durable
  record of tool use.
- Use existing bindings and secrets for credentials instead of tool-specific
  secret plumbing.
- Keep product-specific tool behavior out of the exoharness substrate.
- Support three tool sources: built-in, library, and agent.
- Make it possible for an agent to create a local tool as code without needing a
  package distribution system.

## Non-Goals

- Do not make exoharness a registry of specific app semantics.
- Do not require all tool execution to cross the Rust `execute_tool` protocol.
- Do not choose one model provider's tool schema as the internal source of
  truth.
- Do not design a standardized tool marketplace or package distribution system
  yet.
- Do not add product-specific event variants, binding kinds, or storage records
  for individual tools.

## Current Shape

The current TypeScript surface has a small `ToolDefinition`:

```ts
interface ToolDefinition {
  name: string;
  description: string;
  parameters: JsonValue;
}
```

`exoharness/examples/typescript/basic-harness.ts` exposes only
`buildShellToolDefinitions(context.conversationConfig)`. When the model calls a
tool, the TypeScript runner sends an `execute_tool` runtime request to Rust.
Rust's `BasicToolRuntime` currently dispatches only `shell`, backed by the
conversation sandbox.

That is a good host-backed boundary for built-in tools that need Rust-owned
runtime behavior. It should not be the only tool execution path. TypeScript can
already access events, artifacts, bindings, secrets, and sandbox processes
through `TurnContext`, so many tools can execute directly in TypeScript while
still using exoharness for durable and privileged operations.

## Tool Sources

### Built-In

Built-in tools are maintained by the maintainers of `exo`. They are part of the
core release, reviewed with the project, documented with the harness, and
updated as `exo` evolves.

Examples:

- `shell`
- `run_workspace_command`, if we choose to ship it as a first-party tool
- future core exoharness inspection or artifact tools

Built-ins can still be optional. A conversation or harness should explicitly
choose which built-ins are exposed to the model.

### Library

Library tools are not written by the agent itself, but they are also not part of
the core `exo` release. They may be written by the user, by a team, or by a
third party.

There is no standardized distribution plan yet. For now, a library tool can be a
local TypeScript module imported by a harness. Later, library tools could be
distributed as npm packages, copied modules, git submodules, or another format.
The architecture should not depend on that choice.

Examples:

- A user-written IRC tool module.
- A team-maintained internal incident-management tool.
- An externally maintained GitHub or Linear tool package.

### Agent

Agent tools are created by the agent itself. The agent may write a TypeScript
module, a script, or another local artifact, then ask the harness to expose it as
a model-facing tool.

Agent tools are the riskiest category because the author is the agent. They
should be clearly marked as `agent`, scoped narrowly, and subject to stricter
policy. For a first implementation, agent-created tools should be local to a
conversation or workspace and should not be promoted into shared library tools
without user review.

## Core Design

### Model Tool Definition

Keep the model-facing definition small and provider-neutral:

```ts
interface ToolDefinition {
  name: string;
  description: string;
  parameters: JsonValue;
  outputSchema?: JsonValue;
}
```

`outputSchema` should be optional. It is useful for tools that return structured
results, but model runtimes can ignore it when the provider has no native
output-schema concept.

Auth requirements, source, policy, runtime choice, and provenance should not be
added to `ToolDefinition`. Those are executor concerns.

### Harness Tool

Add an executor-side representation around the model definition:

```ts
type HarnessToolSource = "built_in" | "library" | "agent";

interface ToolExecutionContext {
  readonly context: TurnContext;
  readonly toolCallId?: string;
}

interface ToolHandler {
  execute(
    args: JsonObject,
    execution: ToolExecutionContext,
  ): Promise<ToolResult>;
}

interface ToolInstance {
  definition: ToolDefinition;
  source: HarnessToolSource;
  handler: ToolHandler;
}
```

This is a TypeScript harness API, not an exoharness API. The executor can attach
policy, tracing, auth, and implementation details without changing the portable
model-facing contract.

### Tool Module

Library and agent tools should use a standardized module shape. Each tool module
should default export a `Tool`. The export name is standardized, so loaders do
not need to guess whether a file exports `createTool`, `ircSendMessageTool`, or
something else. This is the TypeScript equivalent of loading a `.so` file with a
known interface.

Tool modules should separate initialization parameters from runtime parameters:

- Initialization parameters configure the tool before it is exposed. They are
  not model-visible and can include server names, default channels, secret ids,
  allowlists, and other setup values.
- Runtime parameters are the model-facing arguments in `definition.parameters`.
  They are supplied by the model each time it calls the tool.

```ts
interface ToolInitializationContext {
  readonly context: TurnContext;
  readonly source: HarnessToolSource;
}

interface Tool {
  definition: ToolDefinition;
  initializationParameters: JsonValue;
  initialize(
    args: JsonObject,
    initialization: ToolInitializationContext,
  ): Promise<ToolHandler> | ToolHandler;
}
```

The registry or loader combines the module, source, and initialized handler into
a `ToolInstance`:

```ts
async function initializeTool(
  tool: Tool,
  source: HarnessToolSource,
  initializationArgs: JsonObject,
  context: TurnContext,
): Promise<ToolInstance> {
  return {
    definition: tool.definition,
    source,
    handler: await tool.initialize(initializationArgs, {
      context,
      source,
    }),
  };
}
```

This makes the module contract stable while leaving each tool's private
implementation types, such as `IrcConfig`, internal to that module. The harness
can load the module with `await import(path)` and read `module.default`. For
static imports, the equivalent is:

```ts
import * as module from "./foo";

const tool = module.default;
```

### Tool Registry

Add a `HarnessToolRegistry` in `exoharness/typescript/harness/index.ts`:

```ts
const tools = createToolRegistry(context);

tools.useBuiltIns(["shell"]);
tools.register(await loadLibraryTool(context, "irc", ircInitialization));
tools.register(await loadAgentTool(context, "irc_send_message"));

const request = {
  model,
  messages,
  tools: tools.definitions(),
};

const events = await tools.executePending(toolCalls);
```

The registry should:

- Map tool names to `ToolInstance` handlers.
- Reject duplicate tool names at registration time.
- Expose `definitions()` for model calls.
- Execute pending tool calls with streaming `tool_call` and `tool_result`
  updates when enabled.
- Return durable `tool_result` events for the caller to append to the turn.
- Preserve each tool's source so policy and tracing can distinguish built-in,
  library, and agent tools.
- Support registering initialized `Tool` default exports for
  library and agent tools.

The existing `context.executePendingTools` can remain as the host-backed default
for compatibility with simple harnesses. The registry should be the preferred
path for TypeScript harnesses that compose multiple tool sources.

## Execution Paths

### Host-Backed Execution

Some tools should continue to delegate to Rust or another host runtime. `shell`
is the main example today. The TypeScript registry can expose `shell` as a
built-in tool while its handler delegates to:

```ts
context.executeTool({
  functionName: "shell",
  arguments: args,
});
```

This lets Rust continue to own sandbox lifecycle and shell execution while the
TypeScript harness gets a uniform registry API.

### TypeScript Execution

Library and agent tools can often run directly in the TypeScript harness runner.
They can call external APIs, use Node libraries, access generic exoharness
bindings and secrets, write artifacts, and append custom events through the
existing `TurnContext`.

This path is useful for tools where the trusted substrate does not need to know
the protocol semantics.

### Sandboxed Process Execution

Some tools need to run code in a sandbox. A built-in tool such as
`run_workspace_command` can use:

```ts
const process = await context.startSandboxProcess({
  command: [shellProgram, "-lc", command],
});
```

This is useful for running scripts or local programs, but it should be treated
as a powerful built-in capability, not as its own tool source. If we expose it,
it should have explicit policy and should be enabled intentionally.

Before relying on sandboxed execution for untrusted agent-authored code, we
should verify the sandbox security model. If we need strong in-process
JavaScript isolation, a smaller runtime such as QuickJS may be a better fit than
unrestricted Node execution.

## Configuration and Credentials

Tools should use existing generic substrate objects:

- Non-secret configuration can live in harness code, agent config,
  conversation config, artifacts, or future generic installation records.
- Credentials should live in `Secret`.
- References between configuration and credentials should use secret ids or
  existing binding ids.
- Tool definitions should not expose raw credential material.
- Tool result events should not contain raw credential material.

If persisted tool installation state becomes necessary, add a generic record
that does not encode product-specific semantics:

```ts
interface ToolInstallation {
  id: string;
  toolId: string;
  source: "library" | "agent";
  version?: string;
  scope: "exoharness" | "agent" | "conversation";
  initialization: JsonObject;
  bindingIds: string[];
  secretIds: string[];
  enabled?: boolean;
}
```

Do this only when the executor needs persisted tool configuration. The first
implementation can work with explicit imports and local initialization arguments
in the harness.

## Policy

Policy belongs to the executor or harness module. Evaluate it in two places:

- Exposure time: decide whether a tool should be included in
  `tools.definitions()` for the current turn.
- Invocation time: decide whether the exact call can run with the supplied
  arguments, credentials, bindings, mounts, network access, and user/session
  context.

The first implementation can keep policy simple and explicit:

- `shell` is exposed only when `conversationConfig.shellProgram` is set.
- Networked tools require explicit networking enablement.
- Tools with external side effects should have a confirmation hook before
  execution.
- Agent tools should default to the narrowest useful scope.
- Agent tools should not silently persist beyond the conversation or workspace
  where they were created.

The CLI/TUI should render confirmation prompts, but the executor should own the
decision and the durable record of the decision.

## Events and Observability

Keep `tool_requested` and `tool_result` as the canonical history. Model runtime
helpers already append `tool_requested` from model outputs, and registry
execution should return `tool_result` events.

Add optional custom events only when they provide real value:

- `tool_policy_decision`: exposure or invocation allowed/denied.
- `tool_invocation_started`: tool name, source, optional library id/version.
- `tool_invocation_completed`: duration, status, redacted result summary.
- `tool_auth_refreshed`: secret id or binding id, without credential material.

Large logs should be artifacts. Events should contain summaries and references,
not unbounded output or secrets.

Tracing should also preserve the tool source. That makes it possible to compare
built-in, library, and agent tool behavior in Braintrust or other tracing
systems without changing the durable event contract.

## Incremental Implementation Plan

The implementation should move in small, testable steps. The first milestone is
shell parity: the TypeScript basic harness should behave exactly as it does
today, but through the registry. Only after that should we add library and agent
tool loading.

### Step 1: Add Portable Types Only

Add the core TypeScript types in `exoharness/typescript/harness/index.ts`:

- `outputSchema?: JsonValue` on `ToolDefinition`.
- `HarnessToolSource = "built_in" | "library" | "agent"`.
- `ToolExecutionContext`.
- `ToolHandler`.
- `ToolInstance`.
- `ToolInitializationContext`.
- `Tool`.

For Rust, add `output_schema: Option<Value>` to the Rust `ToolDefinition` only
if the Rust model-runtime path needs to deserialize or forward tool definitions
with output schemas. This can be done later if TypeScript-only work does not
touch Rust serialization.

Test checkpoint:

- `pnpm typecheck`
- `cargo test -p executor` if the Rust `ToolDefinition` changes

Expected behavior change: none.

### Step 2: Add Registry Without Switching Harnesses

Add `HarnessToolRegistry` and `createToolRegistry(context)`.

The registry should support:

- `register(tool: ToolInstance)`.
- Duplicate-name rejection.
- `definitions()`.
- `get(name)`.
- `executePending(toolCalls)`, including stream events and `tool_result` event
  construction.

At this point, no harness needs to use it yet.

Test checkpoint:

- Unit tests for duplicate registration.
- Unit tests for `definitions()`.
- Unit tests for `executePending(...)` using a fake in-memory `ToolInstance`.
- `pnpm typecheck`

Expected behavior change: none.

### Step 3: Move Shell Definition Behind A Built-In Tool

Implement a built-in shell `ToolInstance` that delegates execution to the
existing host path:

```ts
context.executeTool({
  functionName: "shell",
  arguments: args,
});
```

Then reimplement `buildShellToolDefinitions(config)` through the built-in shell
helper. Existing callers should still receive the same model-facing shell
definition.

Test checkpoint:

- Existing tests still pass.
- A focused test verifies `buildShellToolDefinitions(...)` returns the same
  shape as before.
- A focused test verifies the shell `ToolInstance` delegates to
  `context.executeTool`.

Expected behavior change: none.

### Step 4: Let Tracing Use A Custom Tool Executor

Update `ResponsesRuntime.traceToolCall(...)` to accept an optional execution
callback:

```ts
execute = (toolCall: PendingToolCall) =>
  context.executePendingTools([toolCall]);
```

The default preserves existing behavior. Registry-aware harnesses can pass:

```ts
(toolCall) => tools.executePending([toolCall]);
```

Test checkpoint:

- Unit test or typecheck proving existing call sites compile unchanged.
- Unit test proving a supplied callback is used.
- `pnpm typecheck`

Expected behavior change: none for existing harnesses.

### Step 5: Switch The Basic TypeScript Harness To Shell Through Registry

Update `exoharness/examples/typescript/basic-harness.ts` to:

- Create a registry once per turn loop.
- Register built-in `shell`.
- Pass `tools.definitions()` to the model.
- Execute tool calls through the registry callback passed to
  `traceToolCall(...)`.

This step should expose only shell, so it should be behaviorally equivalent to
the current basic TypeScript harness.

Test checkpoint:

- `pnpm typecheck`
- Existing TypeScript harness tests or e2e script.
- Manual smoke test: ask the basic TypeScript harness to run a simple shell
  command and verify `tool_requested` / `tool_result` events still appear.

Expected behavior change: none except internal dispatch path.

### Step 6: Prove Direct TypeScript Library Tools

Add one small library tool that does not require Rust. Prefer a harmless local
tool over a networked integration for the first proof, for example:

- `echo_json`
- `now_fixed_for_test`
- `uppercase`

The point is to prove that a `Tool` default export can be initialized and
registered, and that its handler can produce a `tool_result` without
`context.executeTool`.

Test checkpoint:

- Unit test imports the module, initializes it, registers it, and executes it.
- `pnpm typecheck`

Expected behavior change: none unless the example harness opts into this tool.

### Step 7: Add A Local Agent Tool Loading Convention

Add the smallest local convention for agent tools, such as an artifact or config
record containing:

```json
{
  "tools": [
    {
      "modulePath": ".exo/agent-tools/irc.ts",
      "initialization": {}
    }
  ]
}
```

The loader should:

- Import `module.default`.
- Validate it satisfies the `Tool` shape.
- Validate `initialization` against `initializationParameters`.
- Call `initializeTool(...)`.
- Register the resulting `ToolInstance` with source `"agent"`.

This can start as a helper used by the example TypeScript harness rather than a
new exoharness storage feature.

Test checkpoint:

- Unit test with a generated local agent tool module.
- Unit test for a missing default export.
- Unit test for invalid initialization parameters.
- `pnpm typecheck`

Expected behavior change: only conversations/harnesses that opt into agent tool
loading can expose agent tools.

### Step 8: Add An Example IRC Tool

After the local agent tool loading path works, add a concrete IRC tool under an
examples directory, for example `exoharness/examples/typescript/tools/irc.ts`.

This should be an example of the standardized `Tool` default export:

- `definition` exposes the runtime model-facing `irc_send_message` parameters.
- `initializationParameters` exposes setup values such as server, port, nick,
  TLS, allowed channels, and optional password secret id.
- `initialize(...)` validates initialization arguments and returns a
  `ToolHandler`.
- The handler uses the generic secret APIs for credentials and regular
  TypeScript/Node networking for IRC.

This should be committed separately from the core registry and loading changes.
That keeps the review split clean: first prove the tool API, then add a real
example tool that exercises it.

Test checkpoint:

- Unit test imports the IRC tool, validates initialization, initializes it, and
  verifies the model-facing definition.
- A network-free handler test should mock the IRC socket or connection layer.
- `pnpm typecheck`

Expected behavior change: none unless an example harness opts into the IRC tool.

### Step 9: Add Optional Built-In Code Execution

Only after shell parity and direct TypeScript tools work, decide whether to add
`run_workspace_command` as a built-in. If added, treat it as a powerful built-in
capability with explicit enablement and tests.

Test checkpoint:

- Unit tests for argument validation and structured output.
- Sandbox smoke test.
- Manual review of sandbox security assumptions.

Expected behavior change: only when the built-in is explicitly enabled.

### Step 10: Defer Persistent Installation Storage

Do not add `ToolInstallation` storage until a real library or agent tool needs
durable configuration that cannot reasonably live in harness code, agent config,
conversation config, or artifacts.

Test checkpoint:

- None yet. This should remain a later design decision.

Expected behavior change: none.

## Suggested First Patch

The first patch should stop at Step 2:

- Add the TypeScript types.
- Add `HarnessToolRegistry`.
- Add tests for registration, duplicate names, definitions, and execution using
  fake in-memory tools.
- Do not change `exoharness/examples/typescript/basic-harness.ts` yet.
- Do not change Rust unless TypeScript changes force a Rust schema update.

That patch validates the core API without changing runtime behavior. The second
patch can add shell as a built-in registry tool while preserving the old
`buildShellToolDefinitions(...)` behavior. The third patch can switch the basic
TypeScript harness to registry-backed shell execution.

## Open Questions

- What is the confirmation API between executor and CLI/TUI?
- Should `run_workspace_command` be a built-in tool, or should the first built-in
  code execution tool have a narrower interface?
- What local entrypoint should agent-created tools use so the agent does not
  have to modify the main harness module directly?
- Should library tools be loaded only by explicit imports at first, or should
  there be a small manifest format?
- What is the smallest generic installation record needed before adding storage?

## Recommendation

Build the TypeScript registry first. Treat tools as built-in, library, or agent
tools. Keep `shell` on the existing Rust execution path, execute library and
agent tools directly in TypeScript where practical, and keep exoharness focused
on durable substrate responsibilities: events, bindings, secrets, artifacts, and
sandbox execution.

## Example: Agent-Created IRC Tool

This example walks through how an agent could create IRC support as its own
tool. IRC is useful because it needs network access, configuration, and optional
credentials, but it does not require exoharness to learn anything IRC-specific.

Assume the agent wants to expose this model-facing tool:

```ts
{
  name: "irc_send_message",
  description: "Send a message to an IRC channel.",
  parameters: {
    type: "object",
    additionalProperties: false,
    properties: {
      channel: {
        type: "string",
        description: "IRC channel name, for example #exo.",
      },
      text: {
        type: "string",
        description: "Message text to send.",
      },
    },
    required: ["channel", "text"],
  },
  outputSchema: {
    type: "object",
    additionalProperties: false,
    properties: {
      ok: { type: "boolean" },
      server: { type: "string" },
      channel: { type: "string" },
    },
    required: ["ok", "server", "channel"],
  },
}
```

### What the Agent Creates

The agent writes a local tool module, for example
`.exo/agent-tools/irc.ts`:

```ts
import net from "node:net";
import tls from "node:tls";

import type { Tool, JsonObject, ToolResult, TurnContext } from "@exo/harness";

interface IrcConfig {
  server: string;
  port: number;
  nick: string;
  username: string;
  realname: string;
  tls: boolean;
  passwordSecretId?: string | null;
}

const ircTool = {
  definition: {
    name: "irc_send_message",
    description: "Send a message to an IRC channel.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        channel: { type: "string" },
        text: { type: "string" },
      },
      required: ["channel", "text"],
    },
    outputSchema: {
      type: "object",
      additionalProperties: false,
      properties: {
        ok: { type: "boolean" },
        server: { type: "string" },
        channel: { type: "string" },
      },
      required: ["ok", "server", "channel"],
    },
  },
  initializationParameters: {
    type: "object",
    additionalProperties: false,
    properties: {
      server: { type: "string" },
      port: { type: "number" },
      nick: { type: "string" },
      username: { type: "string" },
      realname: { type: "string" },
      tls: { type: "boolean" },
      passwordSecretId: { type: ["string", "null"] },
    },
    required: ["server", "port", "nick", "username", "realname", "tls"],
  },
  initialize(args) {
    const config = parseIrcConfig(args);
    return {
      async execute(args, execution): Promise<ToolResult> {
        return sendIrcMessage(execution.context, config, args);
      },
    };
  },
} satisfies Tool;

export default ircTool;

function parseIrcConfig(args: JsonObject): IrcConfig {
  return {
    server: stringArgument(args, "server"),
    port: numberArgument(args, "port"),
    nick: stringArgument(args, "nick"),
    username: stringArgument(args, "username"),
    realname: stringArgument(args, "realname"),
    tls: booleanArgument(args, "tls"),
    passwordSecretId: optionalStringArgument(args, "passwordSecretId"),
  };
}

async function sendIrcMessage(
  context: TurnContext,
  config: IrcConfig,
  args: JsonObject,
): Promise<ToolResult> {
  const channel = stringArgument(args, "channel");
  const text = stringArgument(args, "text");
  const password = await resolvePassword(context, config.passwordSecretId);

  await withIrcConnection(config, async (socket) => {
    if (password) {
      socket.write(`PASS ${password}\r\n`);
    }
    socket.write(`NICK ${config.nick}\r\n`);
    socket.write(`USER ${config.username} 0 * :${config.realname}\r\n`);
    socket.write(`PRIVMSG ${channel} :${text}\r\n`);
    socket.write("QUIT\r\n");
  });

  return {
    ok: true,
    server: config.server,
    channel,
  };
}

async function resolvePassword(
  context: TurnContext,
  secretId: string | null | undefined,
): Promise<string | null> {
  if (!secretId) {
    return null;
  }
  const secret =
    await context.exoharness.current.conversation.getSecret(secretId);
  if (!secret) {
    throw new Error(`IRC password secret does not exist: ${secretId}`);
  }
  if (secret.type !== "key") {
    throw new Error("IRC password secret must be a key secret");
  }
  return secret.value;
}

async function withIrcConnection(
  config: IrcConfig,
  run: (socket: net.Socket) => Promise<void> | void,
): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    const socket = config.tls
      ? tls.connect(config.port, config.server)
      : net.connect(config.port, config.server);
    socket.setEncoding("utf8");
    socket.setTimeout(10_000);
    socket.once("connect", async () => {
      try {
        await run(socket);
        socket.end(resolve);
      } catch (error) {
        socket.destroy();
        reject(error);
      }
    });
    socket.once("error", reject);
    socket.once("timeout", () => {
      socket.destroy(new Error("IRC connection timed out"));
    });
  });
}

function stringArgument(args: JsonObject, name: string): string {
  const value = args[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`IRC tool argument ${name} must be a non-empty string`);
  }
  return value;
}

function optionalStringArgument(args: JsonObject, name: string): string | null {
  const value = args[name];
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`IRC tool initialization ${name} must be a string`);
  }
  return value;
}

function numberArgument(args: JsonObject, name: string): number {
  const value = args[name];
  if (typeof value !== "number") {
    throw new Error(`IRC tool initialization ${name} must be a number`);
  }
  return value;
}

function booleanArgument(args: JsonObject, name: string): boolean {
  const value = args[name];
  if (typeof value !== "boolean") {
    throw new Error(`IRC tool initialization ${name} must be a boolean`);
  }
  return value;
}
```

This module is ordinary TypeScript harness code. It does not require a Rust tool
implementation because it can run directly inside the TypeScript harness runner.
It uses exoharness only for secret lookup. Its default export is the stable
loader contract. `IrcConfig` can remain internal because the harness passes
untyped JSON initialization parameters and the module validates them.

### What the Harness Needs

The harness needs a local entrypoint for agent-created tools so the agent does
not have to edit the main turn loop every time. A simple first version could be
an explicit loader in `exoharness/examples/typescript/basic-harness.ts`:

```ts
interface AgentToolManifest {
  tools: Array<{
    modulePath: string;
    initialization: JsonObject;
  }>;
}

async function registerAgentTools(
  context: TurnContext,
  tools: HarnessToolRegistry,
): Promise<void> {
  const manifest =
    await context.exoharness.current.conversation.readArtifactJson<AgentToolManifest>(
      {
        artifactId: "agent-tools",
      },
    );
  for (const entry of manifest?.tools ?? []) {
    const module = (await import(entry.modulePath)) as {
      default: Tool;
    };
    tools.register(
      await initializeTool(
        module.default,
        "agent",
        entry.initialization,
        context,
      ),
    );
  }
}
```

`initializeTool` should validate `entry.initialization` against the tool's
`initializationParameters` before calling `initialize(...)`.

The turn loop then builds the registry and loads agent tools before calling the
model:

```ts
const tools = createToolRegistry(context).useBuiltIns(["shell"]);
await registerAgentTools(context, tools);

const request: NativeResponsesRequest = {
  model,
  messages,
  tools: tools.definitions(),
  maxOutputTokens: context.agentConfig.maxOutputTokens,
  metadata: turnMetadata(context),
};
```

The agent would also need to write the manifest artifact:

```json
{
  "tools": [
    {
      "modulePath": ".exo/agent-tools/irc.ts",
      "initialization": {
        "server": "irc.libera.chat",
        "port": 6697,
        "nick": "exo-agent",
        "username": "exo",
        "realname": "Exo Agent",
        "tls": true,
        "passwordSecretId": "irc-password"
      }
    }
  ]
}
```

This is intentionally a minimal local convention, not a distribution system. A
more polished version could validate the manifest, restrict allowed paths, cache
loaded modules, and require user approval before exposing new agent tools.

### Tool Execution Wiring

The harness passes `tools.definitions()` to the model request and executes
returned calls through `tools.executePending(...)`:

```ts
const toolResultEvents = await tools.executePending([toolCall]);
await turn.addEvents(toolResultEvents);
```

If the current runtime helper still hardcodes
`context.executePendingTools(...)`, update `ResponsesRuntime.traceToolCall` to
accept an optional executor callback:

```ts
async traceToolCall(
  turnParent: TraceParent,
  context: TurnContext,
  toolCall: PendingToolCall,
  roundIndex: number,
  execute = (toolCall: PendingToolCall) =>
    context.executePendingTools([toolCall]),
): Promise<EventData[]> {
  return tracedUnderParent(
    turnParent,
    async (span) => {
      const events = await execute(toolCall);
      span.log({ output: toolResultTraceOutput(events) });
      return events;
    },
    // existing trace args
  );
}
```

The harness can then call:

```ts
await runtime.traceToolCall(turnParent, context, toolCall, round, (toolCall) =>
  tools.executePending([toolCall]),
);
```

### Required Configuration

The conversation or agent must have networking enabled because IRC is an
external network call:

```bash
exo agent create --model gpt-5.4 --enable-networking "IRC Agent"
```

If the IRC server requires a password or NickServ token, store it as a normal
secret:

```bash
exo secret set irc-password --env IRC_PASSWORD
```

The exact CLI command may differ as the config surface evolves, but the storage
model should remain generic: the IRC tool references a secret id, and the
exoharness stores only the secret material. The model sees the tool schema and
arguments, not the password.

### What Does Not Change

This example should not require:

- A new exoharness binding type named `irc`.
- A Rust `ToolRuntime` implementation for IRC.
- IRC-specific event variants.
- Raw IRC credentials in model-visible prompts, tool definitions, or events.

The durable event history remains the same:

1. The model emits `irc_send_message`.
2. The executor appends `tool_requested`.
3. The registry authorizes and executes the TypeScript handler.
4. The registry returns a `tool_result` event.
5. The next model round sees the result through normal event materialization.

### Hardening Before Sharing

For a local experiment, the direct module above is enough. Before treating IRC
as a reusable library tool or allowing broad agent-created tools, add:

- A policy check that only allows configured servers and channels.
- A confirmation requirement for sending messages to public channels.
- Rate limits and message length validation.
- Redacted observability events for connection failures.
- Manifest validation and path restrictions for agent tool modules.
- Tests for argument validation, duplicate tool registration, missing secrets,
  disabled networking, and rejected manifests.

After review, a user could promote the IRC implementation from an `agent` tool
to a `library` tool by moving it into a user-maintained module and importing it
explicitly from the harness. The exoharness substrate still only needs generic
bindings, secrets, artifacts, events, and sandbox/network policy.
