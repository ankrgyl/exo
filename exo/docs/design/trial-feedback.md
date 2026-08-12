# Trial Feedback

## Goal

Add a feedback phase to the trial adapter without changing the initial task
environment or mixing one trial's context into another. Exo should reflect on
grader feedback in the filesystem state it submitted, then retain useful
memories, tools, policy changes, and other agent-level learning for later
trials.

This work builds on the trial adapter and Harbor evaluation branches. It should
remain a separate change so the basic trial path can be reviewed independently.

## Lifecycle

At the end of `trial_run`:

1. Exo explicitly sends `trial_complete` for the active request.
2. Before acknowledging completion, the adapter snapshots the attached task
   container.
3. The adapter durably records the snapshot against the trial target.
4. Exo detaches from the evaluator-owned container without stopping or deleting
   it.
5. The adapter returns `trial_complete`, including the snapshot id.

When feedback arrives:

1. The adapter resolves the existing trial conversation and latest snapshot by
   target.
2. Exoharness creates a new Exo-owned sandbox from that snapshot. It does not
   rewind or take ownership of the original container.
3. The restored sandbox becomes the conversation's active sandbox.
4. The adapter wakes the same conversation with the grader feedback and
   reflection instructions. The evaluator should include all feedback it has,
   including rewards, verifier output, and failure details.
5. Rebuild continuations work as they do for the initial trial.
6. When Exo explicitly finishes, the adapter returns `feedback_complete`.

Using the same conversation preserves the reasoning and trajectory for that
trial. Using the same filesystem state lets Exo inspect its submitted work.
Agent-scoped state remains the mechanism by which lessons affect later trials.

## Protocol

The successful `trial_complete` response gains `snapshot_id`:

```json
{
  "type": "trial_complete",
  "request_id": "trial request id",
  "target": "stable trial id",
  "conversation_id": "Exo conversation id",
  "snapshot_id": "snapshot of the submitted task container",
  "summary": "optional short summary"
}
```

Feedback uses a new request id but the same target:

```json
{
  "type": "trial_feedback",
  "request_id": "new unique delivery id",
  "target": "the same stable trial id",
  "instructions": "what Exo should do with the result",
  "feedback": "grader output and other feedback"
}
```

Once the restored sandbox is ready, the adapter emits `feedback_started` with
`request_id`, `target`, `conversation_id`, and the restored `sandbox_id`. This
lets the evaluator retain the trajectory even if reflection times out.

The final response is:

```json
{
  "type": "feedback_complete",
  "request_id": "feedback request id",
  "target": "stable trial id",
  "conversation_id": "the original trial conversation id",
  "summary": "optional short summary"
}
```

Only one phase may be active for a target. Repeating a completed request id
returns its recorded response. The first implementation supports one feedback
phase, starting from the snapshot recorded at trial completion.

The reflection prompt should tell Exo to inspect its prior work and the restored
filesystem, understand what succeeded or failed, and extract general lessons
that will help on similar future tasks. It should explicitly consider durable
memories, reusable tools, and changes to its own policy or implementation. The
goal is learning, not repairing the already-graded submission.

## Exoharness changes

### Snapshot attached sandboxes

`snapshot_sandbox` should support a running attached Docker container. The
provider captures its state without changing ownership, stopping it, or
detaching it. The snapshot and `SandboxSnapshotted` event remain owned by the
conversation.

The existing prohibition on snapshotting attachments is an Exoharness policy
restriction rather than a limitation of the snapshot abstraction. Initial
support can be Docker-only and should return a clear unsupported-provider error
for other attachment types.

### Create a sandbox from a snapshot

Add a scoped operation with the shape:

```text
create_sandbox_from_snapshot(snapshot_id, optional provider and idle timeout)
    -> new sandbox_id
```

It creates a new sandbox record and backend resource from the stored snapshot.
The new sandbox:

- has a new Exoharness sandbox id and provider resource;
- is owned by the same agent or conversation as the snapshot;
- is not an attachment;
- inherits the source sandbox's restore-relevant configuration;
- emits `SandboxCreated` followed by `SandboxStarted` with the source snapshot
  id.

This differs from `start_sandbox`, which restores into an existing sandbox id
and therefore represents rollback. Creating from a snapshot represents a fork
of the environment.

Snapshot manifests must retain enough source configuration to create the new
sandbox even after the evaluator deletes the original attached container.

### Select the restored sandbox

Conversation configuration describes how Exoharness should create a default
sandbox. It should not prevent an explicitly attached or explicitly started
sandbox from being used merely because that sandbox came from a different
image or has different mounts.

Today the executor searches active conversation sandboxes and reuses a created
sandbox only when its stored configuration matches the current conversation
configuration. That check is useful for implicit reuse: it prevents a config
change from silently selecting an incompatible old sandbox. It should remain
for the implicit "find or create my configured sandbox" path.

Explicit lifecycle operations should take precedence. The proposed selection
rule is:

1. Use the most recently explicitly attached or started active sandbox.
2. Otherwise, reuse the newest active created sandbox matching conversation
   configuration.
3. Otherwise, create a sandbox from conversation configuration.

`SandboxStarted` is sufficient to express the restored sandbox becoming
current; a separate trial-specific event or `started_from_snapshot` exception
is unnecessary. Sandbox event replay should treat a later `SandboxStarted` as
moving that active sandbox to the front of selection. The shell and general
conversation-sandbox paths must share this rule rather than implementing
slightly different matching behavior.

## Adapter state

The durable record for each target gains:

```text
conversation_id
latest_snapshot_id
active phase: trial or feedback
active request id
completed request ids and responses
```

The Rust trial runtime owns conversation and snapshot state. The worker owns
socket delivery, request retry, and response replay. A completion response must
not enter the worker outbox until its snapshot id is durably recorded.

If Exo restarts during work or reflection, the worker replays the active phase
against the same conversation. If it restarts while finalizing completion, it
must either return the already-recorded response or finish snapshotting before
returning; it must never acknowledge a response with no recoverable snapshot.

## Ownership and cleanup

- The evaluator owns and cleans up the original task container.
- Exo may run commands in and snapshot that container while it is attached.
- Detaching releases Exo's reference but does not stop or delete the original.
- Exo owns sandboxes created from its snapshots and may stop or delete them.
- Snapshot retention follows Exo state retention. Removing a trial record may
  later garbage-collect snapshots that are no longer referenced.

## Implementation split

The change should be reviewable in two commits:

1. **Exoharness lifecycle support:** snapshot attachments, create a new sandbox
   from a snapshot, and make explicit sandbox starts win selection.
2. **Trial feedback:** protocol types, durable target state, prompts, completion
   snapshotting, feedback restore, and Harbor-side feedback submission/export.

Cancellation and the more general executor-independent trial runner are outside
this change.

## Open questions

- Whether detachment failure after a successful snapshot should fail trial
  completion or be logged as cleanup failure. Snapshot durability must remain
  the hard requirement.
