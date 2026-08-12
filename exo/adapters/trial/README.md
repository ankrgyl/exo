# Trial Adapter

This adapter provides an interface to hand off containerized work to Exo over a local Unix socket. It manages the end-to-end complexities of running a task such as handling rebuild/restart continuations. Eventually it will also enable cancelling a running trial.

Each `target` receives a dedicated conversation, while all trials share the
same persistent Exo agent and therefore retain memories, tools, harness edits,
and other durable learning.

The given container remains owned by the caller, which manages its lifecycle.

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

The caller first receives `trial_started`, then waits for `trial_complete`.
Completion snapshots the submitted container and returns both the Exo
conversation id and snapshot id. Pending requests are persisted and replayed
after an Exo restart; completion requires an explicit `send_adapter_message`
call from the trial conversation.

## Feedback

After grading, the caller may send one feedback request on the same target:

```json
{
  "type": "trial_feedback",
  "request_id": "new unique request id",
  "target": "the same stable trial id",
  "instructions": "how to reflect and retain useful learning",
  "feedback": "rewards, verifier output, and failure details"
}
```

The adapter creates an Exo-owned sandbox from the submission snapshot and
wakes the original conversation. It emits `feedback_started`, then waits for
Exo to explicitly send `feedback_complete`. Feedback does not create another
snapshot.

## Cancellation

When the evaluator times out or otherwise cancels its wait, it sends a
`trial_cancel` control request containing the active `request_id` and `target`.
The worker removes that request from durable pending state and acknowledges
`trial_cancelled`, preventing a deleted task container from being replayed
after later adapter restarts. Cancellation does not yet interrupt an active
Exo turn.
