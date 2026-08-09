# Trial Adapter

This adapter provides an interface to hand off containerized work to Exo over a local Unix socket. It manages the end-to-end complexities of running a task such as handling rebuild/restart continuations. Eventually it will also enable cancelling a running trial.

Each `target` recieves a dedicated convo, while all trials share the same persistent Exo agent so retain memories, tools, harness edits, etc.

The given container is assumed to be owned by the caller, i.e. the responsibolity to manage its lifecycle is on the caller.

There are two classes of events: _trial_run_ and _feedback_.

## Trial Run

The trial run event schema is as follows:

```json
{
  "type": "trial_run",
  "request_id": "unique request id",
  "target": "stable trial id",
  "container_id": "running Docker container id",
  "instructions": "work to perform"
}
```

The caller waits for `trial_complete`. The response includes the Exo
conversation id for trajectory export. Pending requests are persisted and
replayed after an Exo restart; completion requires an explicit
`send_adapter_message` call from the trial conversation.

## Feedback

Feedback is not implemented yet. When implented, it will provide a way for evaluation feedback to be handed off to Exo for reflection and self-improvement. This will use the same conversation, and a container containing the state at submission time.
