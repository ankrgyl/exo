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

## Task boundary

For this evaluation, a task is one externally specified Harbor trial: an
instruction, a starting container, a resource budget, and a verifier that
produces the reward and logs. Each task receives a fresh conversation and
filesystem environment. The Exo agent identity and its agent-scoped memory,
skills, tools, and source changes persist across tasks.

This boundary matters to the experiment. Repeating a task measures recovery
from prior feedback; performance on a new task measures whether the learning
transfers. Neither should be inferred only from the amount of state Exo writes.

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
that will help on similar future tasks. It should route each lesson to the
narrowest durable form that fits: memory for concise cross-task facts and
heuristics, skills for reusable procedures, tools for repeated mechanical work,
and policy or implementation for broad agent behavior. Task-specific details
and unsupported guesses should be discarded. The prompt must not require every
lesson to become a memory, and it should ask Exo to avoid duplicate or
superseded memories. The goal is learning, not repairing the already-graded
submission.

## Learning-router prototype

The Harbor plugin accepts two reflection strategies:

- `memory` preserves the original PR #202 prompt, which asks Exo to put every
  useful general lesson in durable memory and optionally create tools or change
  its implementation.
- `router` asks Exo to choose the narrowest useful destination: memory for a
  stable fact or heuristic, skill for a reusable procedure, tool for a repeated
  deterministic operation, policy or implementation for a broad behavior
  change, and discard for task-specific or unsupported conclusions.

After reflection, Harbor writes `agent/learning.json` beside the trajectory.
The report counts actual successful calls to `remember`, `install_skill`,
`install_agent_tool`, `manage_tool`, and the corresponding removal or restart
tools. It does not treat the reflection summary as proof that learning was
persisted. Discard decisions are intentionally not counted because they have no
observable durable mutation. The report also records successful `use_skill`
calls and calls whose tool-result source is `agent` during the task phase. This
makes later skill and self-created-tool reuse observable. Installs performed in
the same task are tracked and excluded from the prior-task reuse metric; memory
reuse remains implicit because all durable memories are currently injected
into every turn.

At job end, Harbor writes `learning-summary.json` in the job directory. It
aggregates reward; attempted, successful, failed, and unresolved actions by
route; later skill/tool reuse; and the final durable-memory count, with the
fixed model and task name recorded for comparison. An unresolved action was
requested but has no matching result event. Because every comparison run
starts with a fresh Exo root, that final count is also the run's memory growth.
The summary counts memory entries without copying their text. A comparison is
invalid if any completed trial is missing its reflection report; the comparison
runner rejects that arm instead of treating missing instrumentation as zero
learning.

The first controlled comparison should run both strategies from separate fresh
Exo roots **and separate clean source checkouts created from the same commit**,
with the same fixed model, selected tasks, task order, attempt count, and
resource limits. A fresh Exo root alone is insufficient because a genuine
self-edit or workspace-local tool source can otherwise contaminate the next
experimental arm. Compare Harbor reward with memory, skill, tool, and policy
actions from `learning.json`. Same-task reruns measure recovery; a held-out task
set is still required to measure transfer.

One paired run is only a diagnostic probe. Repeat independent comparisons with
both arm orders so stochastic tool behavior, provider variance, and shared
rate-limit timing are not mistaken for effects of the reflection strategy.

The current Harbor attached task sandbox and restored feedback sandbox do not
mount the Exo source checkout. Memory, skills, and legacy generated tools are
actionable, but source-level policy changes are not yet a complete experimental
route. `rebuild_and_restart_exo` without an actual source change must not count
as improvement. Testing policy/code routing requires an isolated writable
checkout integration; it must not share a checkout between the baseline and
router arms.

The bundled `self-evolution-smoke-test` forces tool creation and reuse, so it is
only an integration test. The `learning-router-transfer-test` is the first
router-behavior probe: two fresh task environments apply the same deterministic
record-normalization rule to different inputs, without naming memory, skills,
or tools. Harbor still verifies output externally. The first reflection has an
opportunity to create a reusable artifact; the second task's report shows
whether prior learning was actually reused. This is a targeted transfer probe,
not evidence of broad benchmark improvement. The comparison must reject a run
unless the recorded task sequence places the learn task before the transfer
task; local dataset discovery order is not itself experimental evidence.

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
