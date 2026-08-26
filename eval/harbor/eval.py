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
    "learning-router-transfer-test": "datasets/learning-router-transfer-test",
}
DATASET_TASKS = {
    "terminal-bench-easy": (
        "fix-git",
        "prove-plus-comm",
        "cobol-modernization",
    ),
}
PROVIDERS = {
    "openai": {
        "api_key_env": "OPENAI_API_KEY",
        "base_url": None,
    },
    "openrouter": {
        "api_key_env": "OPENROUTER_API_KEY",
        "base_url": "https://openrouter.ai/api/v1",
    },
}
CONFIG_FIELDS = {
    "dataset",
    "dataset_path",
    "model",
    "n_tasks",
    "n_attempts",
    "include_task_names",
    "reflection_strategy",
    "provider",
    "skip_build",
    "workspace_root",
    "exo_bin",
    "output_root",
}


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
        "reflection_strategy": "router",
        "provider": "openai",
        "skip_build": False,
        "workspace_root": None,
        "exo_bin": None,
        "output_root": None,
    }
    if known.config is not None:
        with known.config.open("rb") as file:
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
            "smoke-test, self-evolution-smoke-test, "
            "learning-router-transfer-test, terminal-bench, "
            "terminal-bench-easy, terminal-bench-sample, terminal-bench-pro, "
            "or name@version"
        ),
    )
    parser.add_argument(
        "--dataset-path",
        type=Path,
        help="local dataset directory, for example an Endless Terminals checkout",
    )
    parser.add_argument("--model")
    parser.add_argument("--provider", choices=tuple(PROVIDERS))
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
        "--reflection-strategy",
        choices=("memory", "router", "lifecycle"),
        help="memory baseline, prompt-only router, or validated learning lifecycle",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the resolved command without running it",
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="reuse target/debug/exo instead of rebuilding it",
    )
    parser.add_argument(
        "--workspace-root",
        type=Path,
        help="isolated Exo source workspace used for agent-local files and self-edits",
    )
    parser.add_argument(
        "--exo-bin",
        type=Path,
        help="existing Exo binary to use with --skip-build",
    )
    parser.add_argument(
        "--output-root",
        type=Path,
        help="directory under which this run's timestamped result directory is created",
    )
    return parser.parse_args()


def dataset_arguments(args: argparse.Namespace) -> list[str]:
    if args.dataset_path is not None:
        dataset_path = Path(args.dataset_path).expanduser().resolve()
        if not dataset_path.is_dir():
            raise ValueError(f"dataset path is not a directory: {dataset_path}")
        return ["--path", str(dataset_path)]
    if local_dataset := LOCAL_DATASETS.get(args.dataset):
        return ["--path", str(Path(__file__).resolve().parent / local_dataset)]
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


def ordered_local_task_paths(args: argparse.Namespace) -> list[Path] | None:
    """Resolve local dataset tasks in an explicit continual-learning order."""
    if args.dataset_path is not None:
        dataset_path = Path(args.dataset_path).expanduser().resolve()
    elif local_dataset := LOCAL_DATASETS.get(args.dataset):
        dataset_path = (Path(__file__).resolve().parent / local_dataset).resolve()
    else:
        return None

    # A direct task path does not need an ordered JobConfig.
    if (dataset_path / "task.toml").is_file():
        return None

    tasks = sorted(
        path.resolve()
        for path in dataset_path.iterdir()
        if path.is_dir() and (path / "task.toml").is_file()
    )
    if args.include_task_names:
        tasks = [
            path
            for path in tasks
            if any(
                fnmatch(_local_task_name(path), pattern)
                for pattern in args.include_task_names
            )
        ]
    if args.n_tasks is not None:
        tasks = tasks[: args.n_tasks]
    if not tasks:
        raise ValueError(f"local dataset has no selected tasks: {dataset_path}")
    return tasks


def _local_task_name(task_path: Path) -> str:
    with (task_path / "task.toml").open("rb") as file:
        task = tomllib.load(file).get("task", {})
    name = task.get("name")
    if not isinstance(name, str) or not name:
        raise ValueError(f"local task has no task.name: {task_path}")
    return name


def write_ordered_task_config(
    args: argparse.Namespace, run_dir: Path
) -> Path | None:
    """Write Harbor's ordered `tasks` list for a local multi-task dataset."""
    tasks = ordered_local_task_paths(args)
    if tasks is None:
        return None
    destination = run_dir / "ordered-tasks.json"
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(
        json.dumps(
            {"tasks": [{"path": str(task)} for task in tasks]},
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    return destination


def slug(value: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", value.lower()).strip("-") or "eval"


def harbor_command(
    args: argparse.Namespace,
    *,
    harbor: Path | str,
    repo: Path,
    exo: Path,
    run_dir: Path,
    job_name: str,
    ordered_tasks_config: Path | None = None,
) -> list[str]:
    task_limit = (
        []
        if args.n_tasks is None or ordered_tasks_config is not None
        else ["--n-tasks", str(args.n_tasks)]
    )
    task_source = (
        ["--config", str(ordered_tasks_config)]
        if ordered_tasks_config is not None
        else dataset_arguments(args)
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
        "--pk",
        "adapter_start_timeout_sec=90",
        "--pk",
        f"reflection_strategy={args.reflection_strategy}",
        "--jobs-dir",
        str(run_dir / "jobs"),
        *task_limit,
        "--job-name",
        job_name,
        "--yes",
        "--debug",
        *task_source,
    ]
    return command


def require_command(name: str) -> str:
    command = shutil.which(name)
    if command is None:
        raise ValueError(f"required command is not on PATH: {name}")
    return command


def print_result_paths(run_dir: Path, job_name: str) -> None:
    job_dir = run_dir / "jobs" / job_name
    print("\n===Results===")
    print(f"Harbor results: {job_dir / 'result.json'}")
    print(f"Learning summary: {job_dir / 'learning-summary.json'}")
    print(f"View: harbor view {run_dir / 'jobs'}")


def main() -> int:
    try:
        args = parse_args()
        if (args.n_tasks is not None and args.n_tasks <= 0) or args.n_attempts <= 0:
            raise ValueError("n_tasks and n_attempts must be positive")

        source_repo = Path(__file__).resolve().parents[2]
        repo = (
            args.workspace_root.expanduser().resolve()
            if args.workspace_root is not None
            else source_repo
        )
        if not repo.is_dir():
            raise ValueError(f"workspace root is not a directory: {repo}")
        exo = (
            args.exo_bin.expanduser().resolve()
            if args.exo_bin is not None
            else repo / "target/debug/exo"
        )
        if args.exo_bin is not None and not args.skip_build:
            raise ValueError("--exo-bin requires --skip-build")
        timestamp = dt.datetime.now(dt.UTC).strftime("%Y%m%dT%H%M%SZ")
        # Keep the Unix socket below Linux's 108-byte path limit. Harbor's job
        # metadata already records the dataset and model.
        output_root = (
            args.output_root.expanduser().resolve()
            if args.output_root is not None
            else source_repo / ".local/harbor-evals"
        )
        run_dir = output_root / timestamp
        task_count = args.n_tasks if args.n_tasks is not None else "all"
        job_name = f"{slug(args.dataset)}-{task_count}-{args.reflection_strategy}"
        ordered_tasks_config = write_ordered_task_config(args, run_dir)
        harbor = Path(sys.executable).with_name("harbor")
        if not harbor.is_file():
            harbor = Path(require_command("harbor"))
        command = harbor_command(
            args,
            harbor=harbor,
            repo=repo,
            exo=exo,
            run_dir=run_dir,
            job_name=job_name,
            ordered_tasks_config=ordered_tasks_config,
        )

        if args.dry_run:
            print(shlex.join(command))
            return 0

        print("\n===Setup===", flush=True)
        provider = PROVIDERS[args.provider]
        api_key_env = provider["api_key_env"]
        if not os.environ.get(api_key_env):
            raise ValueError(f"{api_key_env} is not set")
        required_commands = ["docker", "node", "pnpm"]
        if not args.skip_build:
            required_commands.append("cargo")
        for required in required_commands:
            require_command(required)
        if subprocess.run(
            ["docker", "info"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode:
            raise ValueError("Docker is unavailable; run this through ./eval.sh")

        run_dir.mkdir(parents=True, exist_ok=True)
        if args.skip_build:
            if not exo.is_file():
                raise ValueError(f"--skip-build requires an existing Exo binary: {exo}")
        else:
            subprocess.run(["cargo", "build", "-p", "exo"], cwd=repo, check=True)
        subprocess.run(
            [
                str(exo),
                "--root",
                str(run_dir / "exo"),
                "secret",
                "set",
                args.provider,
                "--env",
                api_key_env,
            ],
            cwd=repo,
            check=True,
        )
        register_command = [
            str(exo),
            "--root",
            str(run_dir / "exo"),
            "model",
            "register",
            args.model,
            "--secret",
            args.provider,
            "--model",
            args.model,
        ]
        if provider["base_url"] is not None:
            register_command.extend(("--base-url", provider["base_url"]))
        subprocess.run(
            register_command,
            cwd=repo,
            check=True,
        )
        print(f"Run directory: {run_dir}")
        print("\n===Trials===", flush=True)
        # Let Harbor own the terminal so its built-in live progress UI works.
        subprocess.run(command, cwd=repo, check=True)
        print_result_paths(run_dir, job_name)
        return 0
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("\nEvaluation stopped.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
