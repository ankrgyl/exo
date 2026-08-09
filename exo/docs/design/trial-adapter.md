# Trial Adapter

## Goal

The trial adapter provides one Exo integration for containerized evaluations
such as Harbor, SRE Gym, and future task runners. An evaluator supplies a
running container and instructions; Exo works until it explicitly declares the
phase complete.

The central purpose is to measure and enable learning across trials. Each trial
gets a fresh conversation and task environment, but all trials run under the
same persistent Exo agent. Agent-level changes therefore carry forward,
including memories, installed or modified tools, prompt and harness changes,
source edits, and any other rewiring Exo performs while working or processing
feedback.

The adapter owns the Exo-side lifecycle. Evaluator-specific code should only
start and grade environments, submit messages, and wait for responses.

## Protocol

The initial protocol has one request.

### `trial_run`

```json
{
  "type": "trial_run",
  "request_id": "unique delivery id",
  "target": "stable trial id",
  "container_id": "running container id",
  "instructions": "work to perform",
  "deadline_at": "optional ISO-8601 timestamp"
}
```

`target` identifies the trial across all of its phases. `request_id` identifies
one delivery and makes retrying a request idempotent. A target may have only one
active phase at a time.

The successful response is:

```json
{
  "type": "trial_complete",
  "request_id": "unique delivery id",
  "target": "stable trial id",
  "conversation_id": "Exo conversation id",
  "summary": "optional short summary"
}
```

### Future: `trial_feedback`

```json
{
  "type": "trial_feedback",
  "request_id": "new unique delivery id",
  "target": "the same stable trial id",
  "instructions": "what Exo should do with the result",
  "feedback": "grader result and other feedback"
}
```

The successful response has the same fields as `trial_complete`, with type
`feedback_complete`, plus the new final `snapshot_id`, allowing more than one
feedback phase if needed.

Feedback and snapshotting are deliberately outside the first implementation.
Cancellation should also be a later request, keyed by `target` and the active
`request_id`. It cancels only that phase; a late completion must not satisfy a
later request.

## Trial lifecycle

For `trial_run`, the adapter:

1. Creates a new conversation for the trial.
2. Attaches the supplied container as that conversation's sandbox.
3. Wakes the conversation with the supplied instructions and the explicit
   completion protocol.
4. Continues waking the same conversation after an Exo rebuild until the agent
   calls `send_adapter_message` to declare completion.
5. Returns `trial_complete` to the waiting evaluator.

In a later version, `trial_feedback` would:

1. Resolve the durable trial record by `target`.
2. Create a sandbox from its latest snapshot and make it active for the existing
   conversation.
3. Wake that conversation with the feedback and new instructions.
4. Handle rebuild continuations until explicit completion.
5. Snapshot the resulting sandbox, update the trial record, and return
   `feedback_complete`.

Ending a model turn is not completion. Only a valid `send_adapter_message` for
the active request completes a phase. In version one, the adapter then responds
immediately. A future feedback implementation will snapshot at this boundary
before responding.

## Durable state

The adapter stores one record per target:

```text
target
conversation_id
active request id and deadline (when active)
completed request ids and responses
```

The current worker persists active requests and completed responses in its
adapter state directory. The runtime durably maps each target to its trial
conversation. A restarted worker can therefore replay a pending wake or return
an already-completed response after Exo rebuilds.

## Learning across trials

Conversation and agent lifetimes are intentionally different:

- A new conversation isolates each trial's immediate context and trajectory.
- The persistent agent retains every durable change made during earlier trials
  and feedback phases.
- The environment snapshot preserves one trial's filesystem for its feedback
  phase; it is not the mechanism that carries learning into unrelated trials.

This separation lets later trials benefit from Exo's accumulated memories,
tools, harness changes, and self-modifications without inheriting another
trial's raw conversation or task container. Feedback processing uses the same
trial conversation and restored snapshot so Exo can inspect its original work,
then materialize useful conclusions into persistent agent state for future
trials.

## Container ownership

The evaluator owns the original running container until it submits the trial.
The ownership boundary must be explicit even though Exo can snapshot an
attached container.

The proposed contract is:

- Exo may execute in and snapshot the supplied container, but does not stop or
  delete it.
- The evaluator remains responsible for cleaning up the original container.
- Sandboxes created from Exo snapshots are Exo-owned.

The core `create_sandbox_from_snapshot` operation creates a new Exo-owned
sandbox without rewinding or reusing the original handle. A feedback design
still needs an explicit, unsurprising way to make that new sandbox active for
the existing conversation.

## Trajectory export

Every completion response includes `conversation_id`. That is the stable source
for exporting the trial's model messages, tool calls, results, and rebuild
continuations.

ATIF conversion should be a separate exporter over the canonical conversation
log rather than part of the adapter protocol. A benchmark bridge can export the
conversation after either completion response and place the result in its own
trial output directory.

## Benchmark bridges

- Harbor resolves its task container, sends `trial_run`, waits, and grades the
  original environment state.
- SRE Gym launches its isolated agent container, sends the same request, and
  retains its native diagnosis, mitigation, and grading mechanisms inside the
  instructions and feedback.

Benchmark-specific fields can be carried as typed optional metadata only when a
shared need emerges. The core protocol should not contain Harbor or SRE Gym
concepts.

## Tradeoffs and open questions

- **Explicit completion is reliable but model-driven.** The evaluator knows
  exactly when Exo is done, but prompts and validation must ensure the tool call
  is made once with the active request id.
- **Snapshots preserve the working context but add lifecycle complexity.** We
  need snapshot support for supplied containers and creation of a new sandbox
  from a snapshot before feedback can use the same filesystem state safely.
- **`container_id` keeps version one simple but assumes local Docker.** If a
  benchmark supplies another environment provider, this field should become a
  typed environment descriptor rather than accumulating provider-specific
  optional fields.
- **Adapter-owned conversation creation is convenient but expands the runtime's
  responsibility.** It needs durable target-to-conversation routing rather than
  the usual fixed adapter conversation.
- **Parallel trials are structurally possible.** Records are keyed by target,
  but concurrent trials that modify shared agent state may make a learning
  evaluation nondeterministic. Evaluators should choose their concurrency.
- **Timeout behavior needs an ownership rule.** The evaluator should send
  cancellation when its deadline expires; the adapter then stops waking that
  phase and rejects late completion without deleting the durable trial record.
