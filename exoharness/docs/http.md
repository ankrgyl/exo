# Exoharness HTTP transport

The HTTP transport exposes the existing exoharness protocol over a single unary JSON endpoint. It is a transport for exoharness primitives, not an executor API and not a model streaming API.

## Base URL

An exoharness HTTP server exposes:

- `GET /health`
- `POST /request`

Clients may be configured with either the server base URL, such as `http://127.0.0.1:4766`, or the full request endpoint, such as `http://127.0.0.1:4766/request`.

## Request

`POST /request`

Headers:

```http
content-type: application/json
accept: application/json
```

Body: `protocol::ClientMessage`.

```json
{
  "kind": "request",
  "id": 1,
  "request": {
    "type": "list_agents"
  }
}
```

The `request` field is one of the tagged variants in `exoharness::protocol::Request`. HTTP does not define a second command schema.

## Response

Success and exoharness-level failures both return HTTP 200 with a `protocol::ServerMessage` body. Malformed HTTP, unsupported methods, or invalid JSON are transport errors and may return non-2xx status codes.

```json
{
  "kind": "response",
  "id": 1,
  "ok": true,
  "response": {
    "type": "agents",
    "agents": []
  },
  "error": null
}
```

On exoharness errors:

```json
{
  "kind": "response",
  "id": 1,
  "ok": false,
  "response": null,
  "error": "agent 019... not found"
}
```

## Handles

Trait objects do not cross HTTP. Stable resources are addressed by their durable ids:

- `agent_id`
- `conversation_id`
- `event_id`
- `artifact_id`
- `binding_id`
- `secret_id`

Turns are addressed by durable ids. `conversation_begin_turn` returns the conversation identity plus the turn record. Subsequent `turn_add_events`, `turn_write_artifact`, and `turn_finish` requests include `agent_id`, `conversation_id`, `session_id`, and `turn_id`; the server reconstructs the turn from those ids.

## Streaming

The first HTTP transport is unary only. Durable writes and reads should stay request/response. Future streaming endpoints should be added separately for observation or process I/O, for example:

- event watches after a cursor
- sandbox process stdout/stderr/stdin

Executor/model streaming is outside this transport.

## Local basic server

The CLI can run the local `BasicExoHarness` behind this HTTP transport:

```bash
exo serve --bind 127.0.0.1:4766
```

Use `-v` or `-vv` to enable stderr tracing output for each exoharness request and response kind:

```bash
exo serve -v --bind 127.0.0.1:4766
```

Another CLI process can use that server instead of opening the local basic backend directly:

```bash
exo --exoharness-url http://127.0.0.1:4766 agent list
```
