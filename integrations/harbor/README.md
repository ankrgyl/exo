# Exo agent for Harbor

This package runs Exo as a Harbor external agent. Harbor starts each Docker task
environment normally; the agent resolves that Compose project's running `main`
container, attaches it to an Exo conversation, and sends `task_started` through
the Harbor adapter. When Exo sends `task_complete`, the agent detaches the
container and returns from `run()`, allowing Harbor to run its verifier.

Install the package in the same Python environment as Harbor:

```bash
pip install -e integrations/harbor
pnpm install
cargo build -p exo
```

Run a task with Harbor's Docker environment and the custom agent import path:

```bash
harbor run \
  --env docker \
  --agent exo_harbor.agent:ExoAgent \
  --model '<harbor-model-label>' \
  --ak exo_repo_root="$PWD" \
  --ak exo_model='<registered-exo-model>'
```

`conversation_mode=per_task` is the default and supports concurrent Harbor
trials. Set `--ak conversation_mode=shared` to retain conversation context
between sequential trials. Both modes reuse the same Exo agent, so agent-level
state remains shared.
