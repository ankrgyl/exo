# Exo agent for Harbor

Runs Exo as a Harbor external agent so it can be evaluated over Harbor's
benchmark catalog. The Exo-side adapter that handles waking Exo's
conversation and waiting on a response lives in the generic
[`exo/adapters/trial`](../../exo/adapters/trial) adapter.

## The split

Harbor has two lifecycle planes relevant to Exo:

1. `BaseAgent`, which is constructed fresh for each trial and controls what
   happens during the trail.
2. `Plugin`, which lives for the duration of a full job and exposes hooks for
   initial agent setup, per-trial completions, and final Exo tear-down.

|                                                            | Owns                                                                |
| ---------------------------------------------------------- | ------------------------------------------------------------------- |
| [`plugin.py`](src/exo_harbor/plugin.py) `ExoSessionPlugin` | Exo's lifetime and the adapter runner. Job-scoped.                  |
| [`agent.py`](src/exo_harbor/agent.py) `ExoAgent`           | One task: resolve the container, submit it, and wait. Trial-scoped. |

Thus the end-to-end lifecycle looks like this:

```
attach_job_plugins(job)
  on_job_start(job)          start Exo, create adapter (fixed slug + socket)
  job.run()
    Trial.create()           new ExoAgent
      setup(env)             resolve Harbor's container     [once per trial]
      run(instruction)       trial_run -> trial_started -> trial_complete
                                                             [once per step]
      <verifier runs>
      TrialEvent.END         restore submitted snapshot and reflect on grading
    ... next trial ...
```

Nit: run() is called per-_step_, which is a construct that exists for some
types of tasks where there are intermediate checkpoints to specify. For most
common task-sets, such as terminal-bench, there is only one default step.

## Responsibilities

Harbor constructs the agent and plugin independently:

- The plugin creates the persistent Exo agent and trial adapter, then starts
  the adapter runner.
- Each Harbor agent resolves its task container, submits it through the shared
  socket, and retains the conversation id from `trial_started`. It exports the
  trajectory after completion or timeout.

After Harbor verifies a completed trial, the plugin sends its rewards,
exception details, and available verifier logs back to the same conversation.
Exo reflects inside a new sandbox restored from the submitted snapshot. The
trajectory is exported again afterward so Harbor shows both the task and its
reflection.

The plugin recovers `exo_root` from `job.config.agents[0].kwargs` rather than
taking its own `--pk` copy, so the path is written down once.

Because nothing is handed over, `setup()` instead probes the adapter socket as
a preflight. That is what catches a run launched with `--agent` but no
`--plugin`: every convention would still resolve to something plausible, but
there would be no worker to accept the trial. A live probe is a stronger check
than a marker file, which can be stale from an earlier run.

## Running

The usual entry point is deliberately small:

```bash
cd eval/harbor
./eval.sh --dataset=terminal-bench
```

It defaults to GPT-5.5, all tasks in the dataset, and one attempt. Flags can
limit or override those defaults:

```bash
./eval.sh --dataset=terminal-bench --model=gpt-5.5 --n-tasks=10 --n-attempts=2
```

OpenRouter is supported with an exact model id. Set `OPENROUTER_API_KEY` and
pass both flags; do not use `openrouter/free` for controlled comparisons because
the underlying model can change between requests:

```bash
./eval.sh --dataset=smoke-test --provider=openrouter \
  --model=z-ai/glm-5.2
```

The learning-router prototype defaults to `router`. Run the memory-first PR
#202 prompt as a baseline with the same fixed model and task selection:

```bash
./eval.sh --dataset=terminal-bench-easy --model=gpt-5.5 \
  --reflection-strategy=memory
./eval.sh --dataset=terminal-bench-easy --model=gpt-5.5 \
  --reflection-strategy=router
```

For a paired comparison, `compare.sh` snapshots the current source into two
identical isolated workspaces, gives each arm a fresh Exo root, runs the same
task sequence with one exact model id, and writes `comparison.json` containing
router-minus-memory reward and reuse deltas:

```bash
./compare.sh --dataset=learning-router-transfer-test \
  --provider=openrouter \
  --model=z-ai/glm-5.2:free
```

Do not pass `openrouter/free`: that router can select a different underlying
model between requests, invalidating the comparison. The isolated workspaces
are retained under `.local/harbor-comparisons` so agent-created tools or source
changes can be inspected after the run.

A single paired run is a diagnostic probe, not a stable performance estimate.
Repeat independent runs with both `--arm-order=memory-first` and
`--arm-order=router-first`; otherwise provider variance, stochastic tool use,
or shared rate-limit timing can be mistaken for a router effect.

When running the arms manually instead of through `compare.sh`, use separate
clean worktrees at the same commit. Separate Exo roots are not sufficient: a
self-edit or workspace-local tool source from the first arm could contaminate
the second.

Each run gets a fresh Exo root. After feedback, every completed trial contains
`agent/learning.json`, which records observable memory, skill, tool, and policy
mutations made during reflection, plus skill loads and agent-created tool calls
during task execution. Each route separates attempted, successful, failed, and
unresolved actions; unresolved means the request had no matching result event.
The job directory also gets `learning-summary.json`, which aggregates reward,
routed actions, later skill/tool reuse, and final memory growth without copying
memory text. `compare.sh` rejects an arm if any trial is missing its reflection
report, so measurement failures cannot silently appear as zero learning.
Compare these reports alongside Harbor reward; artifact count alone is not a
success metric.

Select particular tasks by repeating `--include-task-name`:

```bash
./eval.sh --dataset=terminal-bench \
  --include-task-name=path-tracing \
  --include-task-name=gpt2-codegolf
```

The equivalent TOML field is `include_task_names = ["path-tracing",
"gpt2-codegolf"]`.

For a quick end-to-end check using the bundled tiny task:

```bash
./eval.sh --dataset=smoke-test --n-tasks=1
```

To test self-evolution across trials, run the bundled four-task dataset. Its
first trial asks Exo to install and use a custom tool; its second trial uses
the installed tool again from a fresh conversation and container; its third
trial changes durable agent policy state, rebuilds and restarts Exo, then
finishes the trial after the adapter reconnects; and its fourth intentionally
times out to verify that the partial trajectory is still exported.

```bash
./eval.sh --dataset=self-evolution-smoke-test
```

That dataset explicitly requests tool installation, so it tests mechanics, not
the router's judgment. The bundled transfer pair presents the same
deterministic normalization problem with different data in two fresh
conversations and never mentions a learning route. Use it to check whether
reflection independently creates a narrow artifact and whether the second task
reuses prior learning. The comparison runner verifies that Harbor actually ran
the learn task before the transfer task; a reversed sequence is invalid rather
than a transfer test:

```bash
./eval.sh --dataset=learning-router-transfer-test \
  --reflection-strategy=router
```

When `target/debug/exo` is already built from the current checkout, pass
`--skip-build` to reuse it. This is useful for repeated comparison runs and
avoids growing Rust build artifacts between experimental arms.

For a short real benchmark run, `terminal-bench-easy` selects three Terminal
Bench 2 tasks marked easy: `fix-git`, `prove-plus-comm`, and
`cobol-modernization`.

```bash
./eval.sh --dataset=terminal-bench-easy
```

For repeatable runs, copy [`eval.example.toml`](eval.example.toml), edit it,
and run:

```bash
./eval.sh --config=my-eval.toml
```

Command-line flags take precedence over the config file. Harbor datasets can
also be given as `name@version`. Endless Terminals is not currently in the
Harbor registry, so pass a local checkout with
`--dataset=endless-terminals --dataset-path=/path/to/dataset`.

The wrapper creates `.venv` on its first run. The runner builds Exo, starts the
evaluation, shows Harbor's live task progress, and prints the score and result
paths when it finishes. Runs are saved under `.local/harbor-evals` at the
repository root. Harbor also prints a `harbor view ...` command for opening its
full results UI. Each trial's Trajectory tab shows the Exo rollout, including
model messages, tool calls and results, token usage, and cost. Each trial gets
its own target-scoped Exo conversation.

`--n-concurrent 1` is intentionally fixed so each trial deterministically sees
the persistent agent changes made by all earlier trials.

At `trial_complete`, Exo snapshots and detaches from Harbor's original
container without taking ownership of it. Harbor grades and cleans up that
container normally. Completed trials receive one feedback phase; trials that
time out before producing a snapshot skip reflection but retain their partial
trajectory.

The package is pinned to `harbor>=0.20,<0.21` because the `JobPlugin` protocol
and the `TrialEvent.END` payload may move between Harbor minor releases.
