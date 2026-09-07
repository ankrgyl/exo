# Agent Server

`exo agent-server` exposes Exo's executor harness to a parent process over
JSON-RPC 2.0. It is a long-lived, headless alternative to the interactive CLI:
requests are newline-delimited JSON on stdin, responses and live turn
notifications are newline-delimited JSON on stdout, and logs are written to
stderr. Frames larger than 8 MiB are rejected as parse errors.

```bash
exo agent-server
```

The protocol contract is `exo-agent-server-v1`. Clients must initialize with
protocol version 1 before calling other methods:

```jsonl
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":1,"client":{"name":"demo","version":"0.1.0"}}}
{"jsonrpc":"2.0","id":2,"method":"agent/list","params":{}}
```

The v1 request methods are:

- `initialize`
- `agent/list` and `agent/get`
- `conversation/list`, `conversation/get`, and `conversation/create`
- `turn/start`
- `turn/cancel`, reserved but reported as unsupported in v1

Agent and conversation references use the same ID-or-slug resolution as the
Rust `Harness` facade. Conversation creation accepts `agent_ref` and the
optional native fields `slug`, `name`, `sandbox_image`, `sandbox_provider`, and
`shell_program`.

The server uses one executor runtime, selected by the existing global
`--harness` option (default: `basic`). A turn is rejected when the persisted
agent configuration names a different runtime; restart the server with the
matching `--harness` value.

## Starting a turn

`turn/start` accepts the native serialized Lingua message list and an optional
Exoharness session ID:

```text
{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"agent_ref":"exo-agent","conversation_ref":"dev","input":[{"role":"user","content":"Summarize the repository."}],"session_id":null}}
```

The response acknowledges a process-local `operation_id`. The server then
emits `turn/started`, ordered `turn/event` notifications for executor stream
events, and exactly one `turn/completed` or `turn/failed` notification.
`turn/completed` includes the native `session_id`, `turn_id`, and
`latest_event_id` needed to correlate the live stream with durable Exoharness
history.
Executor failures use a bounded client message; detailed diagnostics remain on
stderr.

Only one Agent Server operation may run on a conversation at a time. Different
conversations may run concurrently. `turn/cancel` remains unadvertised in v1.
The process does own its executor tasks and cancels and joins in-flight turns
when stdin closes or protocol output fails. A stdin closure emits `turn/failed`
with `error.kind: "cancelled"` when stdout is still writable.

## Agent Server versus `exo serve`

`exo serve` is the Exoharness substrate API: it exposes durable agents, events,
artifacts, sandboxes, and related primitives over unary HTTP. `exo agent-server`
is the executor control API: it starts model/tool turns through
`HarnessConversation::send_stream()` and delivers the live executor stream over
stdio.

Agent Server notifications are transient delivery, not a durable replay log.
Exoharness remains the canonical recovery and evidence source.
