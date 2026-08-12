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

To test self-evolution across trials, run the bundled three-task dataset. Its
first trial asks Exo to install and use a custom tool; its second trial uses
the installed tool again from a fresh conversation and container; its third
trial changes durable agent policy state, rebuilds and restarts Exo, then
finishes the trial after the adapter reconnects.

```bash
./eval.sh --dataset=self-evolution-smoke-test
```

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
