# Exoharness

A harness often conflates two different concerns: the infrastructure required to serve an agent, such as message history, secure access to privileged resources, and sandboxes, and the semantics of how the agent thinks and acts, such as compaction, memory, and programmability choices like bash, JavaScript, or SQL tools.

To make that separation explicit, this doc introduces the idea of an **exoharness**: a trusted substrate that manages durable state, brokers access to privileged resources, and provides low-level execution plumbing without owning agent semantics. The exoharness manages the non-semantic substrate.

An **executor** is the layer that owns those semantics: it runs the turn loop, including prompt assembly, model calls, tool use, and memory or compaction policy. A **harness** is the combination of an exoharness and an executor.

The point of making the exoharness minimal is not just architectural cleanliness. It is to maximize the space of behaviors that can evolve above it. By keeping only the irreducible substrate in the exoharness, more of the agent's memory, compaction, orchestration, and execution strategy can itself become programmable and agentic, while the system remains safe and functional.

## What you get

- The **exoharness** gives you durable conversations, sessions, turns, append-only events, artifacts, bindings, secrets, and sandboxes.
- The **executor** still owns the semantics: prompt assembly, model calls, memory and compaction policy, approvals UX, and the turn loop.
- A **harness** is what most people actually use: an executor built on top of an exoharness.

## Architecture

- An **agent** is the high-level behavior that an application aims to implement, including instructions, security policies, and tools.
- An **executor** runs agents on top of the exoharness. It is responsible for turn orchestration (assembling prompts, calling models, handling tool use), and it can hardcode logic or delegate parts of that loop to the LLM (e.g. compaction, interruption, memory policy).
- The **exoharness** manages durable state and brokers access to trusted resources and shared infrastructure, such as sandboxes, auth, and secrets.
- A **harness** combines an exoharness with an executor.

In general, any decision that affects the _semantics_ of the agent belongs in the executor or even the individual agent, for example, when and how to compact messages. On the other hand, the exoharness manages the non-semantic building blocks: history, state, secrets, and sandboxing.

## Data model

### Agent

A global configuration object that contains shared metadata and secrets that applies across conversations with an agent. For example, you could define an MCP server that all users of an agent can access, with a shared, global secret. Of course, such configuration can be scoped to an individual conversation as well.

### Conversation

- A **conversation** is a sequence of interactions with an agent. A conversation can stop and resume, and over time it may accumulate millions of interactions over a very long time. In a coding agent, for example, each time you "resume", you are re-entering a conversation.
- A **session** is one such live set of interactions within a conversation. You can assume that the interactions within a session happen during one active instance of the conversation. In a coding agent, for example, a session would be each series of interactions you have with a conversation while the agent is open.
- A **turn** is one user input, which may map to many LLM calls (e.g. the agent executing a tool loop or farming out work to subagents before producing a final result).
- Explicit session lifecycle still exists, but the common hot path is `beginTurn(...)`. Beginning a turn can durably accept the user's input as part of the same operation. The exoharness returns a turn handle that the executor uses to append events and finish the turn, while head tracking and write ordering stay inside the exoharness.

### Event

An event is an append-only record of a change to conversation state. Events may include executor-emitted LLM inputs and outputs (formatted as [Lingua](https://github.com/braintrustdata/lingua) messages), system updates such as session openings, tool requests and results, and executor-defined custom types. The exoharness stores and orders these events durably; executors interpret them to construct message history and higher-level behavior.

Events are exposed through structured APIs like `getEvents(...)`, `getEvent(...)`, and `watchEvents(...)`. The common path is a typed cursor scan over the conversation log, not raw SQL or payload archaeology. Events use UUIDv7 ids so you can still do ordered comparisons and pagination efficiently.

There is still a low-level append API on conversations, but it should be treated as an escape hatch. Normal executor writes should go through `beginTurn(...)` and the returned turn handle.

Custom event types are allowed, and should generally be namespaced. For example, to implement compaction, an executor can write a custom event that points at a derived context view or summary. Compaction itself does not need to exist as a first-class exoharness concept.

## Time travel

At any given point in time, the entire state of the agent is defined as the version of the event log. That means you can rewind or fork from any point in the event log, and every aspect of the above data model can be recreated at that point in time.

## Primitives

### Artifact

An artifact is an opaque set of bytes that the agent can set and retrieve. Artifacts are immutable and versioned, and these updates are managed through the event log (e.g. `CreateArtifact(path, contents)` returns a version). You can fetch the latest version or a specific version of an artifact.

### Sandbox

Agents benefit from being able to write and run commands in secure environments. The exoharness supports running virtual machines (sandboxes) with pluggable runtimes and gives the agent the ability to run arbitrary commands and provision longer-running sandboxes with lifecycle commands like `create`, `start`, `stop`, and `snapshot`. Sandboxes can be snapshotted, which writes a snapshot id to the event log. An executor could choose to do this after every action, or let the LLM decide when. Snapshots allow time travel to also rewind sandbox state.

### Binding and secret

Bindings are non-secret configuration that refer to secrets. For example, an environment variable binding maps an env var name to a secret, an MCP binding defines a server URL plus optional credentials, and an LLM binding defines a provider/model plus optional credentials.

Secrets hold only credential material, such as opaque keys or OAuth tokens. Executors, agents, and even individual conversations can define bindings and secrets. Conversation-scoped values override agent-scoped values. Secrets can be used without the LLM's knowledge, e.g. for MCP servers, or securely mounted within sandboxes so that certain programs can access them without allowing the LLM to view or expose them directly.

## Execution model

The exoharness stops at the point of executing an LLM call, since to do so, you must make several semantic choices: how to stitch together the prompt, which model to use, and what approvals to solicit from a user before continuing. If the exoharness _could_ call the LLM for you, then necessarily, all of this logic must live within it. Instead, the exoharness helps to solve lower-level problems like fetching message history, storing durable state, handling bindings and secrets, and securely executing code in a sandbox.

An important systems property is that once trusted ingress receives user input, the executor should be able to start the LLM call as quickly as possible. The turn handle is the mechanism for that: `beginTurn(...)` durably accepts the input and returns a handle that the executor uses for subsequent writes. The executor no longer needs to manually manage head ids or multiple round trips before it can start the model call.

Crucially, the durable conversation does not have to equal the prompt. A conversation might contain millions of raw events, while an executor sends only a compacted slice, summary, or derived view to the LLM. Because the raw conversation remains queryable through the exoharness, an agent can revisit older material, inspect a failed compaction, or try a different strategy later. In practice, executors will usually keep their own incremental history caches derived from events, but that caching remains executor policy rather than an exoharness concept. This is the main design space the exoharness opens up.

In many cases, the exoharness itself can be exposed to the LLM, for example to query its own history. That exposure is still configured by the **executor**, which defines the tools available to the model and executes the canonical `while` loop involving LLM calls, tool calls, and user messages. A simple executor loop is:

1. `beginTurn(...)`
2. read or derive prompt history from events
3. call the model
4. append messages and tool requests through the turn handle
5. execute tools and append results through the turn handle
6. `finish()`

An ambitious goal of an exoharness would be to support higher-level harnesses like Claude Code and Codex, by virtualizing their exoharness-like components (e.g. `config.toml`).
