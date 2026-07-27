---
title: Data Model
description: Agents, conversations, sessions, turns, events, and artifacts.
---

# Data Model

## Agent

A global configuration object holding shared metadata and secrets that
apply across all conversations with an agent — for example, an MCP server
every user of the agent can access, with a shared secret. The same
configuration can also be scoped to an individual conversation.

## Conversation, session, turn

- A **conversation** is a sequence of interactions with an agent. It can
  stop and resume, and may accumulate millions of interactions over a very
  long time. In a coding agent, each "resume" re-enters a conversation.
- A **session** is one live set of interactions within a conversation —
  one active instance of it.
- A **turn** is one user input, which may map to many LLM calls (tool
  loops, subagents) before producing a final result.

Explicit session lifecycle exists, but the common hot path is
`beginTurn(...)`: it durably accepts the user's input and returns a **turn
handle** the executor uses to append events and finish the turn. Head
tracking and write ordering stay inside the exoharness, so the executor can
start the model call as quickly as possible after input arrives.

For create/resume/fork/delete and how sessions and turns relate to
adapter wakeups, see [Lifecycles → Conversations](./lifecycles#conversation-lifecycle).

## Event

An event is an append-only record of a change to conversation state:
executor-emitted LLM inputs and outputs (formatted as
[Lingua](https://github.com/braintrustdata/lingua) messages), system
updates like session openings, tool requests and results, and
executor-defined custom types.

The exoharness stores and orders events durably; executors *interpret* them
to construct message history and higher-level behavior. Events use UUIDv7
ids, so ordered comparison and pagination are efficient, and they're
exposed through structured APIs — `getEvents(...)`, `getEvent(...)`,
`watchEvents(...)` — as typed cursor scans, not raw SQL or payload
archaeology.

Custom event types are allowed and should be namespaced. This is how
concepts like compaction stay out of the substrate: an executor writes a
custom event pointing at a derived context view or summary. Compaction
never needs to be a first-class exoharness concept.

Note that the full event log is not the same as the prompt: the executor
decides which events (or which summary of them) to send to the model on a
given round. The log is the durable record; the prompt is a view the
executor builds from it.

## Artifact

An artifact is an opaque, immutable, versioned set of bytes the agent can
set and retrieve. Updates flow through the event log (e.g.
`CreateArtifact(path, contents)` returns a version), and you can fetch the
latest or any specific version.
