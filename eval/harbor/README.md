# Exo agent for Harbor

Runs Exo as a Harbor external agent so it can be evaluated over Harbor's
benchmark catalog, and so it can learn from its grades across trials. Drives
Exo entirely through its CLI.

Harbor integration is done via ExoSessionPlugin + ExoAgent. The plugin provides hooks
for initial Exo setup and post-verification reflection, whereas the ExoAgent handles
a single turn and snapshot.

Each trial gets its own conversation, so no earlier trial's context leaks into
the next one. Learnings appear in the agent-scoped durable state: memory, installed
tools, and Exo's own source code changes. `--n-concurrent 1` is fixed so each trial
deterministically sees what earlier trials learned.

Note: currently this is written to test tthe Exo executor over top of the Exoharness,
but this integration shape supports and policy selected.

## Design Decisions to support Reflection

Harbor manages its containers, Exo only attaches them for runtime. Reflection is
only possible after Harbor finishes grading, so we write the trial to snapshot
its contianer before returning. We use the snapshot of the container for reflection.

Trials that time out before producing a snapshot skip reflection but keep their partial trajectory.

## Running

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

To test self-evolution across trials, run the bundled dataset. It tests custom tools, staying across trials.

```bash
./eval.sh --dataset=self-evolution-smoke-test
```

For a short real benchmark run, `terminal-bench-easy` selects three Terminal
Bench 2 tasks marked easy: `fix-git`, `prove-plus-comm`, and
`cobol-modernization`.

```bash
./eval.sh --dataset=terminal-bench-easy
```

A local dataset directory that contains a `task_order.json` (a JSON list of
task directory names in run order) is treated as an _ordered_ dataset: the
runner passes its tasks to Harbor individually, in that order, instead of as
a `--path` dataset, because Harbor discovers `--path` tasks with
`Path.iterdir()` and the filesystem does not guarantee that order. Ordered
datasets are how continual-learning sequences run — each episode assumes the
agent has seen the previous ones. `--n-tasks=N` takes the first N episodes of
the sequence, and `--n-attempts=2` replays the whole sequence a second time.
For example, with the Continual Learning Bench database-exploration port:

```bash
./eval.sh --dataset=clbench-database-exploration \
  --dataset-path=/path/to/continual-learning-bench/harbor/datasets/database-exploration
```

For repeatable runs, copy [`eval.example.toml`](eval.example.toml), edit it,
and run:

```bash
./eval.sh --config=my-eval.toml
```

## Network policy on SWE-bench

For some reason, Harbor's store of SWE-bench does not disable internet access
in the sandboxes. Exo ends up being smart enough to learn to pull the upstream
patches that actually fixed the issues.

So, to run SWE-bench (and other similar coding tasks) correctly, one needs
to ensure that the following is set for each task in the Harbor .toml file:

```toml
[agent]
network_mode = "no-network"
```

`[verifier]` is left alone, since it installs its own toolchain and needs the
network. Disabling the sandbox's networking is not sufficient on its own:
`web_search` and `web_fetch` run in the harness runner process on the host, and
a shell inside the task container can reach the network by itself.

## Notes

The package is pinned to `harbor>=0.20,<0.21` because the `JobPlugin` protocol
and the `TrialEvent.END` payload may move between Harbor minor releases.

`exo.py` parses ids out of the CLI's human-readable confirmation lines. That
parsing is confined to one function and covered by tests, so a reworded CLI
message fails loudly rather than yielding a wrong id. If that proves annoying,
the fix is a `--json` output mode on the relevant `exo` subcommands.
