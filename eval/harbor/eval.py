#!/usr/bin/env python3
"""Run Exo on a Harbor dataset and print the result."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tomllib
from fnmatch import fnmatch
from pathlib import Path


DATASETS = {
    "terminal-bench": "terminal-bench@2.0",
    "terminal-bench-easy": "terminal-bench@2.0",
    "terminal-bench-sample": "terminal-bench-sample@2.0",
    "terminal-bench-pro": "terminal-bench-pro@1.0",
}
LOCAL_DATASETS = {
    "smoke-test": "datasets/smoke-test",
    "self-evolution-smoke-test": "datasets/self-evolution-smoke-test",
}
DATASET_TASKS = {
    "terminal-bench-easy": (
        "fix-git",
        "prove-plus-comm",
        "cobol-modernization",
    ),
}
CONFIG_FIELDS = {
    "dataset",
    "dataset_path",
    "model",
    "n_tasks",
    "n_attempts",
    "include_task_names",
}


def resolve_config_path(config: Path) -> Path:
    """Find a config given relative to this directory as well as to the cwd.

    eval.sh runs the evaluation from the repository root so Harbor's Docker
    work happens there, which means a bare `--config=my-eval.toml` would
    otherwise resolve against the repo root rather than against the directory
    the file actually sits in next to eval.sh.
    """
    if config.is_file() or config.is_absolute():
        return config
    beside_eval = Path(__file__).resolve().parent / config
    return beside_eval if beside_eval.is_file() else config


def parse_args() -> argparse.Namespace:
    config_parser = argparse.ArgumentParser(add_help=False)
    config_parser.add_argument("--config", type=Path)
    known, _ = config_parser.parse_known_args()

    defaults = {
        "dataset": "terminal-bench",
        "dataset_path": None,
        "model": "gpt-5.5",
        "n_tasks": None,
        "n_attempts": 1,
        "include_task_names": [],
    }
    if known.config is not None:
        with resolve_config_path(known.config).open("rb") as file:
            configured = tomllib.load(file)
        unknown = sorted(set(configured) - CONFIG_FIELDS)
        if unknown:
            raise ValueError(f"unknown config fields: {', '.join(unknown)}")
        defaults.update(configured)

    parser = argparse.ArgumentParser(
        parents=[config_parser],
        description="Run Exo on a Harbor dataset.",
    )
    parser.set_defaults(**defaults)
    parser.add_argument(
        "--dataset",
        help=(
            "smoke-test, self-evolution-smoke-test, terminal-bench, "
            "terminal-bench-easy, terminal-bench-sample, terminal-bench-pro, "
            "or name@version"
        ),
    )
    parser.add_argument(
        "--dataset-path",
        type=Path,
        help=(
            "local dataset directory, for example an Endless Terminals "
            "checkout; a directory with a task_order.json runs its tasks "
            "in that order"
        ),
    )
    parser.add_argument("--model")
    parser.add_argument("--n-tasks", type=int)
    parser.add_argument(
        "--include-task-name",
        dest="include_task_names",
        action="append",
        help="task name to include; may be passed more than once",
    )
    parser.add_argument(
        "--n-attempts", "--number-tries", dest="n_attempts", type=int
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help=(
            "reuse the existing exo binary instead of running cargo build; "
            "for example when another eval is mid-run, since each of its "
            "turns re-invokes the binary a rebuild would replace"
        ),
    )
    parser.add_argument(
        "--agent-network",
        choices=("no-network", "public"),
        default="no-network",
        help=(
            "network policy for the agent phase, written into each task.toml. "
            "Harbor defaults to public, which on SWE-bench lets the agent "
            "fetch the upstream patch for its own task id. The verifier phase "
            "is left alone so it can still install its toolchain."
        ),
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the resolved command without running it",
    )
    return parser.parse_args()


def local_dataset_path(args: argparse.Namespace) -> Path | None:
    if args.dataset_path is not None:
        dataset_path = Path(args.dataset_path).expanduser().resolve()
        if not dataset_path.is_dir():
            raise ValueError(f"dataset path is not a directory: {dataset_path}")
        return dataset_path
    if local_dataset := LOCAL_DATASETS.get(args.dataset):
        return Path(__file__).resolve().parent / local_dataset
    return None


TASK_CACHE_DIR = Path.home() / ".cache/harbor/tasks/packages"
AGENT_NETWORK_MARKER = "network_mode"


def task_config_paths(args: argparse.Namespace, dataset_path: Path | None) -> list[Path]:
    """Every task.toml this run could use.

    Local datasets own their task files. Registry datasets are unpacked into
    Harbor's cache, keyed by the task owner, which is the part of the dataset
    id before the slash.
    """
    if dataset_path is not None:
        return sorted(dataset_path.rglob("task.toml"))
    dataset = DATASETS.get(args.dataset, args.dataset)
    owner = dataset.split("/", 1)[0]
    return sorted((TASK_CACHE_DIR / owner).rglob("task.toml"))


def set_agent_network_mode(text: str, mode: str | None) -> str:
    """Add, replace, or drop `network_mode` in a task.toml's [agent] section.

    Harbor's default network policy is `public`, and these task files say
    nothing about networking, so the agent phase reaches the internet. On a
    benchmark whose task ids are upstream pull request numbers, that lets an
    agent fetch the reference patch instead of solving the task.

    `mode=None` removes the override so the file returns to whatever it
    shipped with, which is what makes the two arms of a comparison honest:
    the same cached file cannot silently keep a previous run's policy.
    """
    lines = text.splitlines()
    output: list[str] = []
    in_agent = False
    wrote = False

    def close_agent_section() -> None:
        """Append the setting after the section's last real line.

        Appending at the raw section end would put it after the blank line
        that separates sections, leaving it visually orphaned against the
        next header even though TOML still reads it as part of [agent].
        """
        nonlocal wrote
        blanks: list[str] = []
        while output and not output[-1].strip():
            blanks.append(output.pop())
        output.append(f'{AGENT_NETWORK_MARKER} = "{mode}"')
        output.extend(reversed(blanks))
        wrote = True

    for line in lines:
        stripped = line.strip()
        if stripped.startswith("["):
            if in_agent and mode is not None and not wrote:
                close_agent_section()
            in_agent = stripped == "[agent]"
        if in_agent and stripped.startswith(AGENT_NETWORK_MARKER):
            # Drop the old value; a replacement is appended below if wanted.
            continue
        output.append(line)
    if in_agent and mode is not None and not wrote:
        close_agent_section()
    if mode is not None and not wrote:
        output.extend(["", "[agent]", f'{AGENT_NETWORK_MARKER} = "{mode}"'])
    return "\n".join(output) + "\n"


def apply_agent_network_mode(paths: list[Path], mode: str | None) -> int:
    """Rewrite only the task files whose policy actually changes."""
    changed = 0
    for path in paths:
        text = path.read_text()
        updated = set_agent_network_mode(text, mode)
        if updated != text:
            path.write_text(updated)
            changed += 1
    return changed


def is_ordered_dataset(dataset_path: Path | None) -> bool:
    """A local dataset that declares the order its tasks must run in.

    Harbor discovers a `--path` dataset's tasks with `Path.iterdir()`, whose
    order the filesystem does not guarantee, and runs trials in discovery
    order. That is fine for independent tasks, but continual-learning
    datasets are sequences: each episode assumes the agent has seen the
    previous ones. Such datasets ship a `task_order.json` listing task
    directory names in run order.
    """
    return dataset_path is not None and (dataset_path / "task_order.json").is_file()


def ordered_task_names(dataset_path: Path, args: argparse.Namespace) -> list[str]:
    names = [str(name) for name in json.loads((dataset_path / "task_order.json").read_text())]
    if args.include_task_names:
        names = [
            name
            for name in names
            if any(fnmatch(name, pattern) for pattern in args.include_task_names)
        ]
        if not names:
            raise ValueError(
                f"no tasks in {dataset_path / 'task_order.json'} match "
                f"{args.include_task_names}"
            )
    if args.n_tasks is not None:
        names = names[: args.n_tasks]
    missing = [
        name for name in names if not (dataset_path / name / "task.toml").is_file()
    ]
    if missing:
        raise ValueError(
            f"task_order.json names tasks that do not exist in {dataset_path}: "
            f"{', '.join(missing)}"
        )
    return names


def ordered_dataset_arguments(
    dataset_path: Path, args: argparse.Namespace, run_dir: Path
) -> list[str]:
    """Pass an ordered dataset's tasks explicitly, in task_order.json order.

    A Harbor job config's `tasks` list is the one task source whose order
    Harbor preserves into trial order (`--n-concurrent 1` then runs them
    strictly in sequence), so the order file becomes a generated job config
    holding one task path per episode. Task filters are applied here — to the
    ordered list, before truncation — because Harbor's own `--n-tasks` and
    `--include-task-name` only apply to dataset sources.
    """
    names = ordered_task_names(dataset_path, args)
    config = {"tasks": [{"path": str(dataset_path / name)} for name in names]}
    run_dir.mkdir(parents=True, exist_ok=True)
    config_path = run_dir / "ordered-tasks.json"
    config_path.write_text(json.dumps(config, indent=2) + "\n")
    return ["--config", str(config_path)]


def dataset_arguments(
    args: argparse.Namespace, run_dir: Path | None = None
) -> list[str]:
    if (dataset_path := local_dataset_path(args)) is not None:
        if is_ordered_dataset(dataset_path):
            if run_dir is None:
                raise ValueError(
                    "ordered datasets need a run directory for the generated "
                    "task-list config"
                )
            return ordered_dataset_arguments(dataset_path, args, run_dir)
        arguments = ["--path", str(dataset_path)]
        # A local checkout can hold a whole benchmark, so the same filtering
        # the registry path supports has to work here. Harbor matches these as
        # globs against the task name, which for a task file without an
        # explicit [task] name is its directory name.
        for task in args.include_task_names:
            arguments.extend(["--include-task-name", task])
        return arguments
    if args.dataset == "endless-terminals":
        raise ValueError(
            "Endless Terminals is not in Harbor's registry; pass --dataset-path"
        )
    dataset = DATASETS.get(args.dataset, args.dataset)
    if not re.fullmatch(r"[^@\s]+@[^@\s]+", dataset):
        raise ValueError(
            f"unknown dataset {args.dataset!r}; use a built-in name or name@version"
        )
    arguments = ["--dataset", dataset]
    for task in (*DATASET_TASKS.get(args.dataset, ()), *args.include_task_names):
        arguments.extend(["--include-task-name", task])
    return arguments


def slug(value: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", value.lower()).strip("-") or "eval"


def harbor_command(
    args: argparse.Namespace,
    *,
    harbor: Path | str,
    repo: Path,
    exo: Path,
    run_dir: Path,
    jobs_dir: Path,
    job_name: str,
) -> list[str]:
    # Ordered datasets resolve --n-tasks themselves (Harbor's flag only
    # limits dataset sources, not an explicit task list).
    ordered = is_ordered_dataset(local_dataset_path(args))
    task_limit = (
        []
        if args.n_tasks is None or ordered
        else ["--n-tasks", str(args.n_tasks)]
    )
    command = [
        str(harbor),
        "run",
        "--env",
        "docker",
        "--n-concurrent",
        "1",
        "--n-attempts",
        str(args.n_attempts),
        "--agent",
        "exo_harbor.agent:ExoAgent",
        "--plugin",
        "exo_harbor.plugin:ExoSessionPlugin",
        "--model",
        args.model,
        "--ak",
        f"exo_repo_root={repo}",
        "--ak",
        f"exo_root={run_dir / 'exo'}",
        "--ak",
        f"exo_bin={exo}",
        "--ak",
        f"exo_model={args.model}",
        "--jobs-dir",
        str(jobs_dir),
        *task_limit,
        "--job-name",
        job_name,
        "--yes",
        "--debug",
        *dataset_arguments(args, run_dir),
    ]
    return command


def require_command(name: str) -> str:
    command = shutil.which(name)
    if command is None:
        raise ValueError(f"required command is not on PATH: {name}")
    return command


def print_result_paths(jobs_dir: Path, job_name: str) -> None:
    print("\n===Results===")
    print(f"Harbor results: {jobs_dir / job_name / 'result.json'}")
    print(f"View: harbor view {jobs_dir}")


def main() -> int:
    try:
        args = parse_args()
        if (args.n_tasks is not None and args.n_tasks <= 0) or args.n_attempts <= 0:
            raise ValueError("n_tasks and n_attempts must be positive")

        repo = Path(__file__).resolve().parents[2]
        exo = repo / "target/debug/exo"
        timestamp = dt.datetime.now(dt.UTC).strftime("%Y%m%dT%H%M%SZ")
        # The Exo state root is per run so every run starts from a fresh agent
        # and its conversations and snapshots stay self-contained.
        run_dir = repo / ".local/harbor-evals" / timestamp
        # Results are not: Harbor's viewer browses one folder of job
        # directories, so keeping them together means `harbor view` can be
        # left open and new runs simply appear in it. That makes the job name
        # the thing that has to be unique.
        jobs_dir = repo / ".local/harbor-evals/jobs"
        task_count = args.n_tasks if args.n_tasks is not None else "all"
        job_name = f"{timestamp}-{slug(args.dataset)}-{task_count}"
        harbor = Path(sys.executable).with_name("harbor")
        if not harbor.is_file():
            harbor = Path(require_command("harbor"))
        command = harbor_command(
            args,
            harbor=harbor,
            repo=repo,
            exo=exo,
            run_dir=run_dir,
            jobs_dir=jobs_dir,
            job_name=job_name,
        )

        if args.dry_run:
            print(shlex.join(command))
            return 0

        print("\n===Setup===", flush=True)
        if not os.environ.get("OPENAI_API_KEY"):
            raise ValueError("OPENAI_API_KEY is not set")
        build_tools = () if args.skip_build else ("cargo", "node", "pnpm")
        for required in ("docker", *build_tools):
            require_command(required)
        if subprocess.run(
            ["docker", "info"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode:
            raise ValueError("Docker is unavailable; run this through ./eval.sh")

        # Ordered datasets already created run_dir for their task-list config.
        run_dir.mkdir(parents=True, exist_ok=True)
        if args.skip_build:
            if not exo.is_file():
                raise ValueError(
                    f"--skip-build requires an existing exo binary at {exo}"
                )
        else:
            subprocess.run(["cargo", "build", "-p", "exo"], cwd=repo, check=True)
        subprocess.run(
            [
                str(exo),
                "--root",
                str(run_dir / "exo"),
                "secret",
                "set",
                "openai",
                "--env",
                "OPENAI_API_KEY",
            ],
            cwd=repo,
            check=True,
        )
        subprocess.run(
            [
                str(exo),
                "--root",
                str(run_dir / "exo"),
                "model",
                "register",
                args.model,
                "--secret",
                "openai",
                "--model",
                args.model,
            ],
            cwd=repo,
            check=True,
        )
        print(f"Run directory: {run_dir}")

        # Applied every run so a re-downloaded task package cannot quietly
        # revert to Harbor's public default, and so switching arms takes
        # effect rather than inheriting the previous run's policy.
        network_mode = None if args.agent_network == "public" else args.agent_network
        dataset_path = local_dataset_path(args)
        task_configs = task_config_paths(args, dataset_path)
        changed = apply_agent_network_mode(task_configs, network_mode)
        print(
            f"Agent network: {args.agent_network} "
            f"({changed} of {len(task_configs)} task files updated)"
        )
        if not task_configs and dataset_path is None:
            # Registry packages are unpacked during the job, so a cold cache
            # means these tasks run before anything could be rewritten.
            print(
                "warning: no cached task packages found, so the agent network "
                "policy could not be applied. Re-run once the dataset has been "
                "downloaded to enforce it.",
                file=sys.stderr,
            )

        print("\n===Trials===", flush=True)
        # Let Harbor own the terminal so its built-in live progress UI works.
        subprocess.run(command, cwd=repo, check=True)
        print_result_paths(jobs_dir, job_name)
        return 0
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("\nEvaluation stopped.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
