#!/usr/bin/env python3
"""Run Exo on a Harbor dataset and print the result."""

from __future__ import annotations

import argparse
import datetime as dt
import os
import re
import shlex
import shutil
import subprocess
import sys
import tomllib
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
            "smoke-test, self-evolution-smoke-test, terminal-bench, "
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
    parser.add_argument("--n-tasks", type=int)
    parser.add_argument(
        "--n-attempts", "--number-tries", dest="n_attempts", type=int
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the resolved command without running it",
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
    for task in DATASET_TASKS.get(args.dataset, ()):
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
    job_name: str,
) -> list[str]:
    task_limit = [] if args.n_tasks is None else ["--n-tasks", str(args.n_tasks)]
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
        "--jobs-dir",
        str(run_dir / "jobs"),
        *task_limit,
        "--job-name",
        job_name,
        "--yes",
        "--debug",
        *dataset_arguments(args),
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
    print(f"View: harbor view {run_dir / 'jobs'}")


def main() -> int:
    try:
        args = parse_args()
        if (args.n_tasks is not None and args.n_tasks <= 0) or args.n_attempts <= 0:
            raise ValueError("n_tasks and n_attempts must be positive")

        repo = Path(__file__).resolve().parents[2]
        exo = repo / "target/debug/exo"
        timestamp = dt.datetime.now(dt.UTC).strftime("%Y%m%dT%H%M%SZ")
        # Keep the Unix socket below Linux's 108-byte path limit. Harbor's job
        # metadata already records the dataset and model.
        run_dir = repo / ".local/harbor-evals" / timestamp
        task_count = args.n_tasks if args.n_tasks is not None else "all"
        job_name = f"{slug(args.dataset)}-{task_count}"
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
        )

        if args.dry_run:
            print(shlex.join(command))
            return 0

        print("\n===Setup===", flush=True)
        if not os.environ.get("OPENAI_API_KEY"):
            raise ValueError("OPENAI_API_KEY is not set")
        for required in ("cargo", "docker", "node", "pnpm"):
            require_command(required)
        if subprocess.run(
            ["docker", "info"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode:
            raise ValueError("Docker is unavailable; run this through ./eval.sh")

        run_dir.mkdir(parents=True)
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
