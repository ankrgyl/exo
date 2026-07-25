# Harbor Integration

Harbor is an evaluation framework that ships with terminal bench, the comes with many benchmarks built-in. This will allow us to run Exo over 100s of existing benchmarks with near-zero integration cost. without writing a separate
adapter for every benchmark.

This document describes an initial local-Docker integration. Harbor remains
responsible for building, starting, verifying, and destroying task containers.
Exo remains responsible for agent policy, durable state, self-improvement, and
conversation execution.

## Why the external-agent interface fits Exo

Exo is well-suited to running over Harbor because it separates its execution environment from the policy (executor) environment. This maps directly to the [Harbor external agent](https://www.harborframework.com/docs/agents#external-agents) interface, which is for "agents which interface with the environment through the BaseEnvironment interface, typically by executing bash commands via the exec method." Thus, the Exo agent stays alive wth all self-improvement intact, across task container cycling an teardowns.

In this setup, Harbor runs an external `ExoAgent`. For each trial, that agent attaches
Harbor's already-running Docker container to an Exo conversation as a borrowed
conversation sandbox. Exo's normal `shell` tool then executes directly in the
task container. Control messages between Harbor and Exo travel through an Exo
Harbor adapter.

Thus, the split of responsibilities is...

Harbor
 - Pulls containerized tasks
 - Starts up Exo via BaseAgent `setup`
   - requires adding a tool to declare "done" (perhaps via an adapter!!!)
 - Starts individual containers and wakes Exo (via the BaseAgent interface `run`) to solve them.
   - Can be done with the same convo, or new convo.
   - Requires adding the ability to set a convo's sandbox directly.
 - Runs evaluator when Exo is done, to grade
 - Provides feedback to Exo

Exo
 - Wakes up to work on a task in a sandbox, notifies Harbor when done
 - Processes learnings about how it did, self-reflects, sends a final feedback_processed message, allowing Harbor to detach the sandbox and advance the queue.

## Ownership and responsibilities


SANDBOX PROBLEM: Exo *needs* to be able to snapshot and rewind in a way friendly to Harbor. Is there any way for us to have Harbor use the final convo sandbox when evaluating? That way, if Exo decides to snapshot, or rewind its sandbox (which I believe starts a new docker container under the hood, PLEASE CHECK THIS), it is the correct one that gets evaluated in the end.
====everything below is outdated, the line above me needs consideration=====

### Harbor

- Resolves benchmark datasets and task definitions.
- Builds and starts each task's Docker environment.
- Selects or creates the Exo conversation for the trial.
- Resolves the running Docker Compose `main` container and attaches it to the
  conversation as a borrowed sandbox.
- Sends the task to Exo and waits for Exo to declare it complete.
- Runs the task verifier after Exo completes.
- Sends the reward and verifier output back to Exo for reflection.
- Waits for Exo to process the feedback before advancing to the next task.
- Stops and deletes task containers.

### Exo

- Owns the agent, executor, conversations, canonical event history, memory,
  installed tools, source changes, and persistent agent sandbox.
- Wakes in response to Harbor adapter messages.
- Solves a task with its normal `shell` tool, which targets the borrowed Harbor
  container while the task is active.
- Explicitly reports task completion to Harbor.
- Processes verifier feedback in the persistent agent sandbox.
- Materializes useful learning in agent-level state, such as memory, tools,
  prompts, or code.
- Explicitly acknowledges that feedback processing is complete.

### Borrowed sandbox

The Harbor container remains exclusively owned by Harbor. Attaching it to
Exoharness grants Exo permission to execute commands in it, but does not
transfer lifecycle ownership. Exo must not stop, delete, restore, snapshot, or
otherwise replace a borrowed sandbox.

Harbor is also responsible for detaching the container. Detachment must be
idempotent so cleanup is safe after success, timeout, cancellation, or process
failure.

## Trial lifecycle

Harbor starts the environment before it invokes the external agent:

```text
Harbor Job
  |
  | create DockerEnvironment
  | docker compose build
  | docker compose up --detach --wait
  v
ExoAgent.setup(environment)
  |
  | ensure Exo and the Harbor adapter are available
  | select/create a conversation
  | resolve the Compose main container id
  | attach it as a borrowed conversation sandbox
  v
ExoAgent.run(instruction, environment, context)
  |
  | send task_started
  | wait for task_complete
  v
Harbor verifier
  |
  | grade the resulting container state
  v
Job.on_trial_ended(...)
  |
  | detach the stopped borrowed sandbox
  | restore agent-sandbox execution
  | send verification_result
  | wait for feedback_processed
  v
Next trial
```

Harbor's job runner owns the sequence of trials. A `BaseAgent` instance handles
one trial; it does not run the overall task queue.

The integration initially requires Harbor concurrency to be one. This ensures
that there is only one active borrowed task environment per shared Exo
conversation and that verifier feedback is processed before the next task
begins.

## `ExoAgent.setup`

`BaseAgent.setup()` is called after Harbor has started the trial environment
and completed its health check. Setup must be idempotent and rely on durable Exo
state rather than Python object lifetime.

On each trial, setup:

1. Ensures the Exo services needed by the Harbor adapter are running.
2. Selects or creates the conversation according to the configured conversation
   mode.
3. Ensures that conversation has an enabled Harbor adapter binding.
4. Resolves the Docker container ID for the Harbor environment.
5. Attaches the container to that conversation as a borrowed sandbox.

The first setup may create the Exo agent and shared conversation. In `shared`
mode it also creates one adapter binding that later trials reuse. In `per_task`
mode each new conversation receives its own adapter binding. Later setups
inspect and reuse the applicable records stored under `.exo`. Concurrent or
repeated setup calls must converge on the same state rather than creating
duplicate adapters or shared conversations.

## Conversation modes

The Harbor agent exposes a `conversation_mode` option with two values:

- `shared`: reuse one conversation for every trial. This preserves raw
  conversational context in addition to agent-level learning.
- `per_task`: create a new conversation for each trial. This gives every task a
  clean context window while retaining agent-level memory, installed tools,
  source changes, and the persistent agent sandbox.

Neither mode changes model weights. Exo only learns across tasks when it
materializes a conclusion in durable agent-level state. The two modes let an
evaluation distinguish improvements caused by conversational context from
improvements caused by durable self-modification.

## Resolving the local Docker container

Version one supports only Harbor's local Docker environment and requires
`--env docker`. Harbor's `DockerEnvironment.start()` runs Docker Compose with a
project name derived from the public `environment.session_id`, and the primary
task container is the Compose `main` service.

`ExoAgent.setup()` runs on the host, where the Docker CLI is already available
to Harbor. It applies Harbor's Compose-project normalization to
`environment.session_id`:

1. Convert the value to lowercase.
2. Prefix `0` if the first character is not alphanumeric.
3. Replace characters outside `[a-z0-9_-]` with `-`.

It then resolves the running container:

```bash
docker ps \
  --filter "label=com.docker.compose.project=${PROJECT}" \
  --filter "label=com.docker.compose.service=main" \
  --format '{{.ID}}'
```

The lookup must return exactly one running container. Zero results mean the
environment is unavailable; multiple results are ambiguous and must fail the
trial rather than attaching an arbitrary container. The integration should
inspect the selected container once more before attachment to confirm that its
Compose labels still match the expected project and service.

The container ID should not be discovered with `BaseEnvironment.exec()`.
`exec()` runs inside the task container, where obtaining the host Docker
identity is unreliable. Container discovery belongs in the host-side external
agent.

This lookup uses only the public `BaseEnvironment.session_id` and standard
Docker Compose labels. It requires no changes to Harbor and no access to
Harbor's private Docker implementation.

## Exoharness sandbox attachment

Exoharness needs a way to bind an existing local Docker container to a
conversation without creating or owning it. The intended CLI surface is:

```bash
exo conversation sandbox attach \
  <agent> <conversation> \
  --provider docker \
  --external-id <container-id> \
  [--default-workdir <path>]
```

`SandboxHandle::attach_sandbox` accepts a typed `SandboxAttachment` descriptor.
The first descriptor is `DockerContainer { container_id }`. Attaching validates
the external resource, allocates an Exo sandbox ID, persists the mapping,
constructs the backend handle, and records `SandboxAttached` in the
conversation's canonical history. The Exo sandbox ID remains distinct from the
Docker container ID.

While attached, the conversation's normal `shell` tool targets the borrowed
sandbox. Sandbox lifecycle operations that imply ownership must reject borrowed
handles.

The matching detach operation is:

```bash
exo conversation sandbox detach <agent> <conversation> <exo-sandbox-id>
```

`SandboxHandle::detach_sandbox` removes the sandbox from active use without
stopping it, records `SandboxDetached`, and returns a typed attachment
descriptor. Detach works for both externally attached containers and
Exo-created Docker sandboxes; in the latter case it transfers lifecycle
responsibility to the caller. Event-log resolution then falls back to the most
recent created or attached sandbox that has not been stopped or detached.

Attachments are runtime capabilities, not durable promises that a particular
Docker container will survive a restart. On recovery, Exoharness must validate
that the referenced container still exists and still has the expected Harbor
labels before using it.

## Harbor adapter

The Harbor adapter is an Exo adapter type. It provides the control plane between
the host-side Harbor integration and Exo's conversation wakeup machinery.

It accepts inbound Harbor events, wakes the bound conversation, and exposes
Exo's existing `send_adapter_message` tool for correlated outbound events. A
separate `finish_task` tool is not required. Adapter records remain
conversation-scoped: shared mode reuses one record, while per-task mode creates
one for each task conversation.

Every message includes a Harbor trial ID. The adapter rejects stale messages
and routes outbound messages to the waiter for that exact trial.

### Inbound events

`task_started`

- Trial ID and task name.
- Task instruction.
- Conversation ID.
- Attached Exoharness sandbox ID.
- Deadline or remaining task budget.

The wakeup tells Exo that its normal `shell` currently targets the Harbor task
environment and that it must send `task_complete` when it is ready for
verification.

`verification_result`

- Trial ID and task name.
- Reward values.
- Full verifier stdout and stderr.
- Harbor exception information, if verification failed infrastructurally.
- References to the task conversation and relevant Exo event range.

This event is delivered only after the borrowed task sandbox has been detached
and the conversation can use the persistent agent sandbox.

### Outbound events

`task_complete`

- Trial ID.
- Optional short completion summary.

The adapter delivers this to the waiting `ExoAgent.run()`. Receipt causes
`run()` to return, which is Harbor's normal signal to begin verification.

`feedback_processed`

- Trial ID.
- Optional summary of durable changes Exo made in response to the result.

The trial-ended hook waits for this acknowledgement before allowing the next
trial to begin.

Duplicate outbound events are idempotent. An event for the wrong trial ID never
satisfies the current waiter.

## Harbor external agent

The `ExoAgent` implementation maps a single Harbor trial onto the adapter and
borrowed-sandbox APIs.

`setup()` performs the idempotent initialization and attachment described
above. `run()` sends `task_started` and waits for `task_complete`. It also
records the Exo conversation, turn, and event IDs in Harbor's agent metadata so
the two histories can be correlated.

If Exo produces an ordinary final model response without sending
`task_complete`, the trial remains active. The adapter may wake Exo with a
continuation reminder until the Harbor agent timeout is reached. The explicit
adapter event, not the presence of assistant text, is the completion boundary.

Timeout, cancellation, adapter disconnect, Exo failure, and invalid attachment
must surface as Harbor agent errors. Cleanup still detaches the borrowed
sandbox, but only Harbor decides whether to preserve or destroy the task
container.

## Verification feedback hook

Harbor verification occurs after `ExoAgent.run()` returns, so the `BaseAgent`
cannot itself deliver the verifier result. The continual runner therefore uses
Harbor's programmatic `Job` API and registers an awaited
`Job.on_trial_ended(...)` callback.

The callback receives the completed `TrialResult`. It:

1. Detaches the borrowed sandbox if cleanup has not already done so.
2. Restores the selected conversation to agent-sandbox execution.
3. Sends `verification_result` through the Harbor adapter.
4. Waits for the matching `feedback_processed` response.
5. Writes the feedback acknowledgement and Exo event IDs to a feedback sidecar
   in the Harbor trial directory.
6. Returns, allowing the single-concurrency Harbor job to advance.

Harbor emits the trial-ended event after finalizing the environment, so feedback
does not depend on the task container remaining alive. The verifier output and
the durable Exo/Harbor records are the inputs to reflection. Harbor has already
persisted its standard trial result when this callback runs, so the integration
uses a sidecar rather than mutating `result.json` after finalization.

If the trial ends before verification, the same callback sends an
infrastructure-failure result to Exo. Feedback timeout is recorded separately
from task correctness so an Exo reflection failure is not mistaken for a failed
benchmark task.

## Recovery and safety

- Exo state under `.exo` is preserved across trials and Exo process restarts.
- Harbor remains the sole owner of task-container cleanup.
- Attach, detach, adapter setup, and completion acknowledgements are
  idempotent.
- Trial IDs correlate all attachment, task, verifier, and feedback records.
- A borrowed handle is validated before every reattachment after a process
  restart.
- Cancellation detaches the Exo handle but does not issue Docker lifecycle
  commands.
- Stale adapter events cannot wake a different trial's waiter.
- Task correctness, infrastructure failure, Exo runtime failure, and feedback
  failure are recorded as separate outcomes.

## Things to build

1. **Borrowed Docker sandbox support in Exoharness**
   - Attach an already-running Docker container to a conversation.
   - Execute processes with the normal sandbox interface.
   - Reject ownership-only lifecycle operations.
   - Detach without stopping the container.

2. **Exo Harbor adapter**
   - Accept `task_started` and `verification_result`.
   - Emit and correlate `task_complete` and `feedback_processed`.
   - Wake the correct conversation and reject stale trial IDs.

3. **Harbor external `ExoAgent`**
   - Idempotently ensure Exo and the adapter are available.
   - Support `shared` and `per_task` conversation modes.
   - Resolve the local Docker container through Compose labels.
   - Attach it, deliver the task, and wait for explicit completion.

4. **Continual Harbor runner**
   - Force `--env docker` behavior and concurrency one.
   - Construct the Harbor `Job`.
   - Register the trial-ended feedback callback.
   - Wait for Exo reflection before advancing through the task sequence.

## Acceptance criteria

- A known Harbor task can be solved by Exo using its normal `shell` tool in
  Harbor's container.
- Exoharness never creates, stops, or deletes the borrowed Harbor container.
- Harbor verification begins only after `task_complete`.
- The full verifier result reaches Exo, and Harbor waits for
  `feedback_processed`.
- Both conversation modes work across at least two sequential tasks.
- Agent-level memory, installed tools, and source changes survive in
  `per_task` mode.
- Timeouts, cancellation, Exo restart, duplicate events, and stale trial IDs
  leave no active borrowed attachment and do not delete the Harbor container.
