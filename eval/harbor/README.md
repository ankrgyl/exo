# Exo agent for Harbor

This package runs Exo as a Harbor external agent. Harbor starts each Docker task
environment normally; the agent resolves that Compose project's running `main`
container, attaches it to an Exo conversation, and sends `task_started` through
the Harbor adapter. When Exo sends `task_complete`, the agent detaches the
container and returns from `run()`, allowing Harbor to run its verifier.

## Lifecycle

1. Harbor starts the task's Docker environment.
2. On first setup, `ExoAgent` creates the Exo agent and a dedicated setup
   conversation, which configures one long-running Harbor adapter.
3. Each task selects a separate work conversation, attaches Harbor's running
   `main` container, and sends `task_started` to that conversation.
4. Exo works in the container, replies with `task_complete`, and the agent
   detaches the container so Harbor can run its verifier.
5. `ContinualExoPlugin` routes the verification result back to the same work
   conversation and waits for `feedback_processed` before Harbor advances.

## Running Harbor x Exo

Install the package in the same Python environment as Harbor:

```bash
pip install -e eval/harbor
pnpm install
cargo build -p exo
```

Run a task with Harbor's Docker environment and the custom agent import path:

```bash
harbor run \
  --env docker \
  --n-concurrent 1 \
  --agent exo_harbor.agent:ExoAgent \
  --plugin exo_harbor.continual:ContinualExoPlugin \
  --model '<harbor-model-label>' \
  --ak exo_repo_root="$PWD" \
  --ak exo_model='<registered-exo-model>'
```

`--ak` passes an agent constructor argument. `conversation_mode=per_task` is
the default and creates a fresh work conversation for each task; set
`--ak conversation_mode=shared` to reuse one work conversation and retain its
context. Neither mode uses the adapter's setup conversation for task work.

`exo_harbor.continual:ContinualExoPlugin` is Harbor's `module:class` import
path for the job plugin. The plugin receives Harbor's awaited trial-ended hook,
sends rewards and verifier logs back to Exo, waits for `feedback_processed`,
and writes `exo-feedback.json` beside Harbor's `result.json`. It requires
Docker and `--n-concurrent 1` to preserve the ordered lifecycle.
