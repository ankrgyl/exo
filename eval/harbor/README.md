# Exo agent for Harbor

This package runs Exo as a Harbor external agent. Harbor starts each Docker task
environment normally; the agent resolves that Compose project's running `main`
container, attaches it to an Exo conversation, and sends `task_started` through
the Harbor adapter. When Exo sends `task_complete`, the agent detaches the
container and returns from `run()`, allowing Harbor to run its verifier.

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
  --ak exo_model='<registered-exo-model>' \
  --pk exo_repo_root="$PWD"
```

`conversation_mode=per_task` is the default and supports concurrent Harbor
trials. Set `--ak conversation_mode=shared` to retain conversation context
between sequential trials. Both modes reuse the same Exo agent, so agent-level
state remains shared.

The continual plugin receives Harbor's awaited trial-ended hook. It sends
rewards and verifier logs back to Exo, waits for `feedback_processed`, and
writes `exo-feedback.json` beside Harbor's `result.json` before the next trial
starts. The plugin requires Docker and `--n-concurrent 1`; this preserves the
ordered learn-and-advance lifecycle.
